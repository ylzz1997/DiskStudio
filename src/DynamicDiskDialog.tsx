import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { formatSize } from "./types";

export type DynamicDiskInput =
  | { kind: "images"; sectorSize: number }
  | { kind: "disks"; members: string[] };

export type DynamicDiskCandidate = {
  target: string;
  number: number;
  model: string;
  size: number;
  knownOffline: boolean;
};

type Availability = "complete" | "degraded" | "missing" | "unclean";

export type DynamicDiskView = {
  sessionId: string;
  id: string;
  name: string;
  sectorSize: number;
  canPreviewBasicConversion: boolean;
  volumes: {
    id: string;
    name: string;
    kind: "simple" | "spanned" | "striped" | "mirrored" | "raid5";
    size: number;
    availability: Availability;
    memberCount: number;
    missingMembers: number;
  }[];
};

export type DynamicConversion = {
  token: string;
  disk: number;
  model: string;
  volumes: { name: string; offset: number; size: number; filesystem: string }[];
};

export type DynamicConversionOutcome = {
  verifiedBytes: number;
  partitions: number;
};

type Props = {
  candidates: DynamicDiskCandidate[] | null;
  busy: boolean;
  error: string;
  view: DynamicDiskView | null;
  conversion: DynamicConversion | null;
  converted: DynamicConversionOutcome | null;
  conversionProgress: { done: number; total: number } | null;
  onClose: () => void;
  onReselect: () => void;
  onLoadCandidates: () => void;
  onPreviewConversion: (sessionId: string) => void;
  onExecuteConversion: (token: string) => void;
  onCancelConversion: () => void;
  onInspect: (source: DynamicDiskInput) => void;
  onBrowse: (
    sessionId: string,
    volume: string,
    deleted: boolean,
    allowDegraded: boolean,
  ) => void;
};

export function DynamicDiskDialog({
  candidates,
  busy,
  error,
  view,
  conversion,
  converted,
  conversionProgress,
  onClose,
  onReselect,
  onLoadCandidates,
  onPreviewConversion,
  onExecuteConversion,
  onCancelConversion,
  onInspect,
  onBrowse,
}: Props) {
  const { t, catalogText } = useLocale();
  const [kind, setKind] = useState<DynamicDiskInput["kind"]>("images");
  const [sectorSize, setSectorSize] = useState(512);
  const [selected, setSelected] = useState<string[]>([]);
  const [deleted, setDeleted] = useState(false);
  const [allowDegraded, setAllowDegraded] = useState(false);
  const [conversionConfirmed, setConversionConfirmed] = useState(false);
  const offered = candidates ?? [];
  const members = offered
    .map(({ target }) => target)
    .filter((target) => selected.includes(target));
  const volumeKinds = {
    simple: t("简单卷"),
    spanned: t("跨区卷"),
    striped: t("带区卷"),
    mirrored: t("镜像卷"),
    raid5: "RAID-5",
  };
  const states: Record<Availability, string> = {
    complete: t("成员齐全"),
    degraded: t("可降级读取"),
    missing: t("缺少所需成员"),
    unclean: t("元数据需要恢复"),
  };
  return (
    <Dialog
      title={t(conversion || converted ? "转换为基本磁盘" : "浏览动态卷")}
      subtitle={conversion || converted ? undefined : t("选择同一 Windows 动态磁盘组的成员。")}
      wide
      onClose={() => {
        if (!busy) onClose();
      }}
    >
      {converted ? (
        <section className="lost-results">
          <h3>{t("动态盘已转为 GPT 基本磁盘，卷内容 SHA-256 校验一致。磁盘保持脱机。")}</h3>
          <p>
            {t("数据分区")} {converted.partitions} · {formatSize(converted.verifiedBytes)}
          </p>
        </section>
      ) : conversion ? (
        <section className="lost-results">
          <h3>{t("磁盘")} {conversion.disk} · {conversion.model}</h3>
          <p>{t("每个简单卷会成为同一位置和大小的 GPT 数据分区。应用会完整校验转换前后的卷内容，磁盘保持脱机。")}</p>
          {conversion.volumes.map((volume) => (
            <div className="data-action-row" key={volume.offset}>
              <span><strong>{volume.name}</strong> · {volume.filesystem}</span>
              <span>{t("起始偏移")} {formatSize(volume.offset)} · {formatSize(volume.size)}</span>
            </div>
          ))}
          <label className="table-backup-option">
            <input type="checkbox" checked={conversionConfirmed} disabled={busy}
              onChange={(event) => setConversionConfirmed(event.currentTarget.checked)} />
            <span>{t("我已核对目标和范围，并了解数据影响")}</span>
          </label>
        </section>
      ) : view ? (
        <section className="lost-results">
          <h3>{view.name || view.id}</h3>
          <p className="lost-note">
            {t("逻辑扇区")} · {formatSize(view.sectorSize)}
          </p>
          <label className="table-backup-option">
            <input
              type="checkbox"
              disabled={busy}
              checked={deleted}
              onChange={(event) => setDeleted(event.currentTarget.checked)}
            />
            <span>{t("找回删除文件")}</span>
          </label>
          {view.volumes.some(
            (volume) => volume.availability === "degraded",
          ) && (
            <label className="table-backup-option">
              <input
                type="checkbox"
                disabled={busy}
                checked={allowDegraded}
                onChange={(event) =>
                  setAllowDegraded(event.currentTarget.checked)
                }
              />
              <span>{t("允许从剩余动态磁盘成员读取")}</span>
            </label>
          )}
          {view.volumes.length === 0 && (
            <p>{t("磁盘组没有已提交的动态卷。")}</p>
          )}
          {view.volumes.map((volume) => (
            <div className="data-action-row" key={volume.id}>
              <span>
                <strong>{volume.name || volume.id}</strong>
                {" · "}
                {volumeKinds[volume.kind]}
                {" · "}
                {formatSize(volume.size)}
                {" · "}
                {volume.memberCount - volume.missingMembers}/
                {volume.memberCount}
                {" · "}
                {states[volume.availability]}
              </span>
              <button
                className="button"
                disabled={
                  busy ||
                  !(
                    volume.availability === "complete" ||
                    (volume.availability === "degraded" && allowDegraded)
                  )
                }
                onClick={() =>
                  onBrowse(view.sessionId, volume.id, deleted, allowDegraded)
                }
              >
                {deleted ? t("开始扫描") : t("浏览文件")}
              </button>
            </div>
          ))}
        </section>
      ) : (
        <>
          <label className="backup-browser-partition">
            {t("来源")}
            <select
              value={kind}
              disabled={busy}
              onChange={(event) => {
                const next = event.currentTarget.value === "disks" ? "disks" : "images";
                setKind(next);
                if (next === "disks" && candidates === null) onLoadCandidates();
              }}
            >
              <option value="images">{t("RAW/IMG 磁盘镜像")}</option>
              <option value="disks">{t("脱机磁盘")}</option>
            </select>
          </label>
          {kind === "images" ? (
            <label className="backup-browser-partition">
              {t("逻辑扇区")}
              <select
                value={sectorSize}
                disabled={busy}
                onChange={(event) =>
                  setSectorSize(Number(event.currentTarget.value))
                }
              >
                <option value={512}>512 B</option>
                <option value={4096}>4 KiB</option>
              </select>
            </label>
          ) : (
            <div className="btrfs-member-options">
              {candidates?.length === 0 && <p>{t("没有可选的脱机磁盘。")}</p>}
              {offered.map(({ target, number, model, size, knownOffline }) => (
                <label key={target}>
                  <input
                    type="checkbox"
                    checked={selected.includes(target)}
                    disabled={busy}
                    onChange={(event) =>
                      setSelected(
                        event.currentTarget.checked
                          ? [...selected, target]
                          : selected.filter((value) => value !== target),
                      )
                    }
                  />
                  <span>
                    {t("磁盘")} {number} · {model} · {formatSize(size)}
                    {!knownOffline && ` · ${t("状态未知")}`}
                  </span>
                </label>
              ))}
              <button className="button" disabled={busy} onClick={onLoadCandidates}>
                {t("刷新信息")}
              </button>
            </div>
          )}
        </>
      )}
      {conversionProgress && conversionProgress.total > 0 ? (
        <div className="data-progress-meta" role="status">
          <progress value={conversionProgress.done} max={conversionProgress.total} />
          <span>{formatSize(conversionProgress.done)} / {formatSize(conversionProgress.total)}</span>
        </div>
      ) : busy && <p role="status">{t("正在准备")}</p>}
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>
          {t(converted ? "关闭对话框" : "取消")}
        </button>
        {converted ? null : conversion ? (
          <>
            <button className="button" disabled={busy} onClick={onCancelConversion}>
              {t("返回")}
            </button>
            <button className="button primary" disabled={busy || !conversionConfirmed}
              onClick={() => onExecuteConversion(conversion.token)}>
              {t("确认并开始")}
            </button>
          </>
        ) : view ? (
          <>
          <button
            className="button"
            disabled={busy}
            onClick={() => {
              setAllowDegraded(false);
              onReselect();
            }}
          >
            {t("重新选择来源")}
          </button>
          {view.canPreviewBasicConversion && (
            <button className="button" disabled={busy} onClick={() => {
              setConversionConfirmed(false);
              onPreviewConversion(view.sessionId);
            }}>
              {t("预览转换为基本磁盘")}
            </button>
          )}
          </>
        ) : (
          <button
            className="button primary"
            disabled={busy || (kind === "disks" && members.length === 0)}
            onClick={() =>
              onInspect(
                kind === "images" ? { kind, sectorSize } : { kind, members },
              )
            }
          >
            {kind === "images" ? t("选择镜像文件") : t("查找动态卷")}
          </button>
        )}
      </div>
    </Dialog>
  );
}

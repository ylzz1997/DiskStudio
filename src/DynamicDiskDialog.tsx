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

type Props = {
  candidates: DynamicDiskCandidate[] | null;
  busy: boolean;
  error: string;
  view: DynamicDiskView | null;
  onClose: () => void;
  onReselect: () => void;
  onLoadCandidates: () => void;
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
  onClose,
  onReselect,
  onLoadCandidates,
  onInspect,
  onBrowse,
}: Props) {
  const { t, catalogText } = useLocale();
  const [kind, setKind] = useState<DynamicDiskInput["kind"]>("images");
  const [sectorSize, setSectorSize] = useState(512);
  const [selected, setSelected] = useState<string[]>([]);
  const [deleted, setDeleted] = useState(false);
  const [allowDegraded, setAllowDegraded] = useState(false);
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
      title={t("浏览动态卷")}
      subtitle={t("选择同一 Windows 动态磁盘组的成员。")}
      wide
      onClose={() => {
        if (!busy) onClose();
      }}
    >
      {view ? (
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
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>
          {t("取消")}
        </button>
        {view ? (
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

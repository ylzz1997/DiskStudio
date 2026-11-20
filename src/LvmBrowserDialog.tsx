import { useState, type ReactNode } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle } from "./LocalDiskView";
import type { Translate } from "./i18n/translate";
import { formatSize, type Disk } from "./types";

export type LvmMember = {
  target: string;
  layoutKey: string;
  partition: number | null;
};
export function physicalMemberOptions(disks: Disk[], t: Translate) {
  return disks.flatMap((disk) => {
    const hardware = disk.hardware;
    if (disk.kind !== "physical" || !hardware?.identity || !hardware.layoutKey)
      return [];
    const name = diskTitle(disk, t);
    const scopes = disk.partitions
      .filter((part) => part.number !== undefined && !part.system)
      .map((part) => ({
        partition: part.number!,
        label: t("分区 {number}", { number: part.number! }),
        size: part.size,
      }));
    const whole =
      !hardware.isSystem && !disk.partitions.some((part) => part.system && part.role !== "MSR 保留")
        ? [{ partition: null, label: t("整个磁盘"), size: disk.size }]
        : [];
    return [...whole, ...scopes].map(({ partition, label, size }) => {
      const source = {
        target: hardware.identity!,
        layoutKey: hardware.layoutKey!,
        partition,
      };
      return {
        key: JSON.stringify(source),
        source,
        label: `${name} · ${label} · ${formatSize(size)}`,
      };
    });
  });
}
export type LvmView = {
  id: string;
  name: string;
  sequence: number;
  memberCount: number;
  suppliedMembers: number;
  allowDegradedRaid: boolean;
  extentSize: number;
  volumes: {
    id: string;
    name: string;
    size: number;
    profile:
      | "linear"
      | "striped"
      | "mixed"
      | "raid0"
      | "raid1"
      | "raid4"
      | "raid5"
      | "raid6"
      | "raid10"
      | "thin"
      | "thinSnapshot"
      | "thinPool"
      | "snapshot"
      | "unsupported";
    readable: boolean;
    degradedRaid: boolean;
    reason: string | null;
  }[];
};

export function LvmVolumes({
  view,
  children,
  action,
}: {
  view: LvmView;
  children?: ReactNode;
  action: (volume: LvmView["volumes"][number]) => ReactNode;
}) {
  const { t, catalogText } = useLocale();
  const profiles = {
    linear: t("线性"),
    striped: t("条带"),
    mixed: t("线性与条带"),
    raid0: "RAID 0",
    raid1: "RAID 1",
    raid4: "RAID 4",
    raid5: "RAID 5",
    raid6: "RAID 6",
    raid10: "RAID 10",
    thin: t("薄置备卷"),
    thinSnapshot: t("薄置备快照"),
    thinPool: t("薄池"),
    snapshot: t("写时复制快照"),
    unsupported: t("不支持"),
  };
  return (
    <section className="lost-results">
      <h3>{view.name}</h3>
      <p className="lost-note">
        {t("已核对 {selected}/{total} 个物理卷 · 分配单元 {size}", {
          selected: view.suppliedMembers,
          total: view.memberCount,
          size: formatSize(view.extentSize),
        })}
      </p>
      {!view.volumes.length && <p>{t("卷组中没有逻辑卷。")}</p>}
      {view.volumes.some((volume) => volume.readable) && children}
      {view.volumes.map((volume) => (
        <article className="lost-candidate" key={volume.id}>
          <div>
            <strong>{volume.name}</strong>
            <small>{formatSize(volume.size)} · {profiles[volume.profile]}</small>
            {volume.degradedRaid && <small>{t("LVM2 RAID 缺盘读取")}</small>}
            {volume.reason && <small>{catalogText(volume.reason)}</small>}
          </div>
          {volume.readable && action(volume)}
        </article>
      ))}
    </section>
  );
}

export function LvmDegradedOption({ checked, busy, onChange }: {
  checked: boolean;
  busy: boolean;
  onChange: (allowed: boolean) => void;
}) {
  const { t } = useLocale();
  return (
    <label className="table-backup-option">
      <input type="checkbox" checked={checked} disabled={busy}
        onChange={(event) => onChange(event.currentTarget.checked)} />
      <span>{t("允许 LVM2 RAID 缺盘读取")}</span>
    </label>
  );
}

export function LvmBrowserDialog({
  disks,
  primary,
  busy,
  error,
  view,
  onClose,
  onInspect,
  onBrowse,
  onReselect,
}: {
  disks: Disk[];
  primary?: LvmMember;
  busy: boolean;
  error: string;
  view: LvmView | null;
  onClose: () => void;
  onInspect: (members: LvmMember[], allowDegradedRaid: boolean) => void;
  onBrowse: (view: LvmView, volume: string, deleted: boolean) => void;
  onReselect: () => void;
}) {
  const { t, catalogText } = useLocale();
  const [selected, setSelected] = useState<string[]>(
    primary ? [JSON.stringify(primary)] : [],
  );
  const [deleted, setDeleted] = useState(false);
  const [allowDegradedRaid, setAllowDegradedRaid] = useState(view?.allowDegradedRaid ?? false);
  const [filter, setFilter] = useState("");
  const offered = physicalMemberOptions(disks, t);
  const members = offered.filter(({ key }) => selected.includes(key));
  const query = filter.trim().toLocaleLowerCase();
  const visible = offered.filter(({ label }) => label.toLocaleLowerCase().includes(query));
  return (
    <Dialog
      title={t("浏览 LVM2 逻辑卷")}
      subtitle={t("选择同一 LVM2 卷组的物理卷所在分区或磁盘。")}
      wide
      onClose={() => {
        if (!busy) onClose();
      }}
    >
      {view ? (
        <LvmVolumes view={view} action={(volume) => (
          <button
            className="button"
            disabled={busy}
            onClick={() => onBrowse(view, volume.id, deleted)}
          >
            {deleted ? t("开始扫描") : t("浏览文件")}
          </button>
        )}>
          <label className="table-backup-option">
            <input
              type="checkbox"
              checked={deleted}
              disabled={busy}
              onChange={(event) => setDeleted(event.currentTarget.checked)}
            />
            <span>{t("找回删除文件")}</span>
          </label>
        </LvmVolumes>
      ) : (
        <>
          {offered.length > 8 && (
            <label className="backup-browser-partition">
              {t("筛选磁盘或分区")}
              <input type="search" value={filter} disabled={busy}
                onChange={(event) => setFilter(event.currentTarget.value)} />
            </label>
          )}
          <div className="btrfs-member-options">
          {visible.map(({ key, source, label }) => (
            <label key={key}>
              <input
                type="checkbox"
                checked={selected.includes(key)}
                disabled={busy}
                onChange={(event) => {
                  const keep = members.filter(
                    (matched) =>
                      matched.key !== key &&
                      (matched.source.target !== source.target ||
                        (matched.source.partition !== null &&
                          source.partition !== null)),
                  );
                  setSelected(
                    event.currentTarget.checked
                      ? [...keep.map((item) => item.key), key]
                      : selected.filter((item) => item !== key),
                  );
                }}
              />
              <span>{label}</span>
            </label>
          ))}
          </div>
          <p className="lost-note">{t("已选 {count} 个来源", { count: members.length })}</p>
          <LvmDegradedOption checked={allowDegradedRaid} busy={busy} onChange={setAllowDegradedRaid} />
        </>
      )}
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onClose}>
          {t("取消")}
        </button>
        {view ? (
          <button className="button" disabled={busy} onClick={onReselect}>
            {t("重新选择来源")}
          </button>
        ) : (
          <button
            className="button primary"
            disabled={busy || !members.length || members.length > 64}
            onClick={() => onInspect(members.map((member) => member.source), allowDegradedRaid)}
          >
            {busy ? t("正在读取 LVM2 元数据…") : t("读取 LVM2 卷组")}
          </button>
        )}
      </div>
    </Dialog>
  );
}

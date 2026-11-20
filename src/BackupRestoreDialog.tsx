import { useState } from "react";
import { backupPointLabel, type BackupInfo } from "./BackupBrowserPanel";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle, partitionTitle } from "./LocalDiskView";
import { formatSize, type Disk } from "./types";

export function BackupRestoreDialog({
  backup,
  partition,
  disks,
  busy,
  error,
  onClose,
  onPrepare,
}: {
  backup: BackupInfo;
  partition: number | null;
  disks: Disk[];
  busy: boolean;
  error: string;
  onClose: () => void;
  onPrepare: (disk: Disk, targetPart: string, resize: boolean) => void;
}) {
  const { t, catalogText } = useLocale();
  const [target, setTarget] = useState("");
  const [targetPart, setTargetPart] = useState("");
  const [resize, setResize] = useState(false);
  const sourcePart = backup.partitions.find((part) => part.id === partition);
  const point = backup.points.find(
    (point) => point.id === backup.selectedPoint,
  );
  const size = sourcePart?.size ?? backup.size;
  const compatibleParts = (disk: Disk) =>
    disk.partitions.filter(
      (part) =>
        part.number !== undefined &&
        !part.system &&
        (sourcePart?.btrfs
          ? part.role === "Linux 数据" && disk.hardware?.offline != null
          : part.role === "数据分区" && !part.hidden) &&
        (resize ? part.size !== size : part.size === size),
    );
  const eligible = disks.filter((disk) => {
    const hardware = disk.hardware;
    return (
      disk.kind === "physical" &&
      !!hardware?.identity &&
      !!hardware.layoutKey &&
      hardware.layoutAvailable &&
      !hardware.isSystem &&
      hardware.deviceReadOnly !== true &&
      !backup.sourceDisks.includes(hardware.identity) &&
      disk.sectorSize === backup.sector &&
      (sourcePart
        ? disk.scheme === "GPT" && compatibleParts(disk).length > 0
        : disk.size === size)
    );
  });
  const selected = eligible.find((disk) => disk.id === target);
  const parts = selected && sourcePart ? compatibleParts(selected) : [];
  const canPreview =
    !!selected &&
    (sourcePart
      ? parts.some((part) => String(part.number) === targetPart)
      : partition === null && backup.kind === "disk");

  return (
    <Dialog
      title={t("还原所选备份还原点")}
      subtitle={backup.name}
      onClose={() => {
        if (!busy) onClose();
      }}
      footer={
        <>
          <button className="button" disabled={busy} onClick={onClose}>
            {t("取消")}
          </button>
          <button
            className="button primary"
            disabled={busy || !canPreview}
            onClick={() => {
              if (selected && canPreview)
                onPrepare(selected, targetPart, resize);
            }}
          >
            {busy ? t("正在核对目标…") : t("预览还原")}
          </button>
        </>
      }
    >
      <dl className="data-plan">
        <dt>{t("还原点")}</dt>
        <dd>{point && backupPointLabel(point, t, catalogText)}</dd>
        <dt>{t("来源")}</dt>
        <dd>
          {sourcePart
            ? backup.kind === "partition"
              ? t("备份分区")
              : sourcePart.name
            : t("整盘备份")}{" "}
          · {formatSize(size)}
        </dd>
      </dl>
      <div className="backup-settings backup-restore-settings">
        {sourcePart && (
          <label>
            <span>
              <strong>{t("还原方式")}</strong>
              <small>{sourcePart.btrfs
                ? t("预览会核对单设备 Btrfs，目标须为普通 GPT Linux 数据分区。")
                : t("调整容量适用于 FAT12/16/32 和 exFAT。")}</small>
            </span>
            <select
              value={resize ? "resize" : "original"}
              disabled={busy}
              onChange={(event) => {
                setResize(event.target.value === "resize");
                setTarget("");
                setTargetPart("");
              }}
            >
              <option value="original">{t("原尺寸还原")}</option>
              <option value="resize">{t("调整容量还原")}</option>
            </select>
          </label>
        )}
        <label>
          <strong>{t("目标磁盘")}</strong>
          <select
            value={target}
            disabled={busy}
            onChange={(event) => {
              setTarget(event.target.value);
              setTargetPart("");
            }}
          >
            <option value="">
              {eligible.length ? t("选择磁盘") : t("无兼容磁盘")}
            </option>
            {eligible.map((disk) => (
              <option key={disk.id} value={disk.id}>
                {diskTitle(disk, t)} · {formatSize(disk.size)}
              </option>
            ))}
          </select>
        </label>
        {sourcePart && (
          <label>
            <strong>{t("目标分区")}</strong>
            <select
              value={targetPart}
              disabled={busy || !selected}
              onChange={(event) => setTargetPart(event.target.value)}
            >
              <option value="">
                {resize ? t("选择不同容量") : t("选择同容量")}
              </option>
              {parts.map((part) => (
                <option key={part.id} value={part.number}>
                  {t("分区 {number}", { number: part.number! })} ·{" "}
                  {partitionTitle(part, catalogText)} · {formatSize(part.size)}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      {error && <FormError message={catalogText(error)} />}
    </Dialog>
  );
}

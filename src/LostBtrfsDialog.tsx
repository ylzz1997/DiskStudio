import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { diskTitle } from "./LocalDiskView";
import type { LostBtrfsChoice, LostCandidate } from "./lostPartitionModel";
import { formatSize, type Disk } from "./types";

export function LostBtrfsDialog({
  disk,
  disks,
  group,
  busy,
  error,
  onBack,
  onRead,
}: {
  disk: Disk;
  disks: Disk[];
  group: { anchor: LostCandidate; choices: LostBtrfsChoice[] };
  busy: boolean;
  error: string;
  onBack: () => void;
  onRead: (others: LostBtrfsChoice[], allowMissing: boolean) => void;
}) {
  const { t, language } = useLocale();
  const [selected, setSelected] = useState<LostBtrfsChoice[]>([]);
  const [allowMissing, setAllowMissing] = useState(false);
  const available = new Map(
    disks
      .filter((item) => item.hardware?.identity)
      .map((item) => [item.hardware!.identity!, item]),
  );
  const choices = group.choices.flatMap((choice) => {
    const memberDisk = available.get(choice.target);
    return memberDisk?.hardware?.layoutKey === choice.layoutKey && memberDisk
      ? [{ choice, memberDisk }]
      : [];
  });
  const active = selected.filter((selection) =>
    choices.some(({ choice }) => choice === selection),
  );
  const total = group.anchor.btrfsMemberCount ?? 0;
  const count = active.length + 1;
  const label = (memberDisk: Disk, start: number, size: number) =>
    `${diskTitle(memberDisk, t)} · ${t("起始 LBA")} ${start.toLocaleString(language)} · ${formatSize(size)}`;
  return (
    <Dialog
      title={t("联合浏览 Btrfs 成员")}
      subtitle={label(
        disk,
        group.anchor.offset / disk.sectorSize,
        group.anchor.size,
      )}
      wide
      onClose={() => {
        if (!busy) onBack();
      }}
    >
      <p className="lost-note">
        {t(
          "从已完成的搜索中选择同一提交的成员。要补找成员，请返回磁盘管理，在其他范围或磁盘搜索。",
        )}
      </p>
      {!!choices.length && (
        <div className="btrfs-member-options">
          {choices.map(({ choice, memberDisk }) => (
            <label key={choice.candidate}>
              <input
                type="checkbox"
                checked={active.includes(choice)}
                disabled={busy}
                onChange={(event) => {
                  const remaining = active.filter(
                    (item) => item.member !== choice.member,
                  );
                  setSelected(
                    event.currentTarget.checked
                      ? [...remaining, choice]
                      : remaining,
                  );
                }}
              />
              <span>{label(memberDisk, choice.startSector, choice.size)}</span>
            </label>
          ))}
          {total > 2 && (
            <label>
              <input
                type="checkbox"
                checked={allowMissing}
                disabled={busy}
                onChange={(event) =>
                  setAllowMissing(event.currentTarget.checked)
                }
              />
              <span>{t("允许缺盘读取（仅限可校验的数据）")}</span>
            </label>
          )}
        </div>
      )}
      {!choices.length && (
        <p className="lost-note">
          {t("当前搜索记录中没有找到可联合读取的其他成员。")}
        </p>
      )}
      <FormError message={error} />
      <div className="dialog-actions">
        <button className="button" disabled={busy} onClick={onBack}>
          {t("返回搜索")}
        </button>
        <button
          className="button primary"
          disabled={
            busy ||
            count < 2 ||
            count > total ||
            (!allowMissing && count !== total)
          }
          onClick={() => onRead(active, allowMissing)}
        >
          {t("读取 Btrfs 成员")} ({count}/{total})
        </button>
      </div>
    </Dialog>
  );
}

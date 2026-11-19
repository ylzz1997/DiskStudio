import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { type ImageBrowseInfo, type ImageBrowseMode } from "./ImageBrowserPanel";
import { formatSize } from "./types";

export type RaidFiles = {
  id: string;
  files: { id: string; name: string; size: number }[];
};
export type RaidView = ImageBrowseInfo & { checkedRows: number; missingMembers: number };
const layouts = [
  ["jbod", "JBOD"],
  ["raid0", "RAID 0"],
  ["raid1", "RAID 1"],
  ["raid10", "RAID 10 (near)"],
  ["raid4", "RAID 4"],
  ["raid5LeftAsymmetric", "RAID 5 · left asymmetric"],
  ["raid5RightAsymmetric", "RAID 5 · right asymmetric"],
  ["raid5LeftSymmetric", "RAID 5 · left symmetric"],
  ["raid5RightSymmetric", "RAID 5 · right symmetric"],
  ["raid5FixedLast", "RAID 5 · parity last"],
  ["raid6ZeroRestart", "RAID 6 · zero restart"],
  ["raid6NRestart", "RAID 6 · N restart"],
  ["raid6NContinue", "RAID 6 · N continue"],
] as const;
type Member = { key: string; file: string | null; start: string; size: string };
export type RaidRequest = {
  id: string;
  config: { layout: string; sector: number; stripe: number };
  options: { kind: "disk" | "partition"; sector: number };
  members: { file: string | null; range: { start: number; size: number } }[];
};

export function VirtualRaidDialog({
  busy, error, files, view, onSelect, onOpen, onScan, onClose, onCloseRaid,
}: {
  busy: boolean;
  error: string;
  files: RaidFiles | null;
  view: RaidView | null;
  onSelect: () => void;
  onOpen: (request: RaidRequest) => void;
  onScan: (partition: number, mode: ImageBrowseMode) => void;
  onClose: () => void;
  onCloseRaid: () => void;
}) {
  const { t, catalogText } = useLocale();
  const [layout, setLayout] = useState<(typeof layouts)[number][0]>("raid0");
  const [sector, setSector] = useState(512);
  const [stripe, setStripe] = useState(64 * 1024);
  const [kind, setKind] = useState<"disk" | "partition">("disk");
  const [members, setMembers] = useState<Member[]>(() => (files?.files ?? []).map((file) => ({
    key: file.id, file: file.id, start: "0", size: String(file.size),
  })));
  const [partition, setPartition] = useState(view?.partitions[0]?.id ?? 0);
  const [mode, setMode] = useState<ImageBrowseMode>("existing");
  const striped = layout !== "jbod" && layout !== "raid1";
  const ready = !!files && members.length >= 2 &&
    members.every(({ start, size }) => /^\d+$/.test(start) && /^\d+$/.test(size) &&
      Number.isSafeInteger(Number(start)) && Number.isSafeInteger(Number(size)) && Number(size) > 0);
  const change = (index: number, patch: Partial<Member>) =>
    setMembers((rows) => rows.map((row, number) => number === index ? { ...row, ...patch } : row));
  const move = (index: number, delta: number) => setMembers((rows) => {
    const next = [...rows];
    [next[index], next[index + delta]] = [next[index + delta], next[index]];
    return next;
  });
  return (
    <Dialog title={t("虚拟 RAID")} wide onClose={() => !busy && onClose()}>
      {view ? (
        <>
          <p>{view.name} · {formatSize(view.size)} · {t("{size} B 扇区", { size: view.options.sector })}</p>
          {view.checkedRows > 0 && <p className="lost-note">{t("通过 {count} 个条带校验样本。", { count: view.checkedRows })}</p>}
          <p className="lost-note">{t("条带校验无法单独确认成员顺序或布局。")}</p>
          {view.missingMembers > 0 && <p className="lost-note">{t("缺少 {count} 个 RAID 成员。", { count: view.missingMembers })}</p>}
          {view.warning && <p className="lost-note">{catalogText(view.warning)}</p>}
          <label className="backup-browser-partition">
            {t("镜像中的分区")}
            <select disabled={busy} value={partition} onChange={(e) => setPartition(Number(e.target.value))}>
              {view.partitions.map((part) => <option key={part.id} value={part.id}>{part.name} · {formatSize(part.size)}</option>)}
            </select>
          </label>
          <label className="backup-browser-partition">
            {t("文件范围")}
            <select value={mode} disabled={busy} onChange={(e) => setMode(e.target.value as ImageBrowseMode)}>
              <option value="existing">{t("现存文件")}</option>
              <option value="allFiles">{t("全部文件")}</option>
              <option value="deleted">{t("找回删除文件")}</option>
            </select>
          </label>
        </>
      ) : (
        <>
          <p className="lost-note">{t("按阵列顺序排列成员，填写每个成员的数据偏移和长度。虚拟 RAID 使用只读来源。")}</p>
          <button className="button" disabled={busy} onClick={onSelect}>{t("选择 RAID 成员镜像")}</button>
          {files && <>
            <div className="image-browser-open-row">
              <label>{t("RAID 布局")}<select value={layout} disabled={busy} onChange={(e) => setLayout(e.target.value as typeof layout)}>
                {layouts.map(([id, name]) => <option key={id} value={id}>{name}</option>)}
              </select></label>
              <label>{t("RAW 扇区")}<select value={sector} disabled={busy} onChange={(e) => setSector(Number(e.target.value))}>
                <option value={512}>512 B</option><option value={4096}>4096 B</option>
              </select></label>
              {striped && <label>{t("条带大小")}<select value={stripe} disabled={busy} onChange={(e) => setStripe(Number(e.target.value))}>
                {[512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576].filter((size) => size >= sector)
                  .map((size) => <option key={size} value={size}>{formatSize(size)}</option>)}
              </select></label>}
              <label>{t("镜像范围")}<select value={kind} disabled={busy} onChange={(e) => setKind(e.target.value as typeof kind)}>
                <option value="disk">{t("整盘镜像（GPT / MBR）")}</option>
                <option value="partition">{t("分区镜像（文件系统）")}</option>
              </select></label>
            </div>
            <ol className="virtual-raid-members">
              {members.map((row, index) => (
                <li key={row.key}>
                  <strong>{files.files.find((file) => file.id === row.file)?.name ?? t("缺失成员")}</strong>
                  <div className="image-browser-open-row">
                    {row.file && <label>{t("数据偏移（字节）")}<input inputMode="numeric" value={row.start} disabled={busy} onChange={(e) => change(index, { start: e.target.value })} /></label>}
                    <label>{t("数据长度（字节）")}<input inputMode="numeric" value={row.size} disabled={busy} onChange={(e) => change(index, { size: e.target.value })} /></label>
                    <button className="button" aria-label={t("上移")} disabled={busy || index === 0} onClick={() => move(index, -1)}>↑</button>
                    <button className="button" aria-label={t("下移")} disabled={busy || index === members.length - 1} onClick={() => move(index, 1)}>↓</button>
                    {!row.file && <button className="button" disabled={busy} onClick={() => setMembers(members.filter(({ key }) => key !== row.key))}>{t("移除")}</button>}
                  </div>
                </li>
              ))}
            </ol>
            <button className="button" disabled={busy || members.length >= 64} onClick={() =>
              setMembers([...members, { key: crypto.randomUUID(), file: null, start: "0", size: members[0]?.size ?? "" }])}>
              {t("添加缺失成员")}
            </button>
          </>}
        </>
      )}
      <FormError message={catalogText(error)} />
      <div className="dialog-actions">
        {view && <button className="button" disabled={busy} onClick={onCloseRaid}>{t("关闭阵列")}</button>}
        <button className="button" disabled={busy} onClick={onClose}>{t("取消")}</button>
        {view ? (
          <button className="button primary" disabled={busy || !view.partitions.length} onClick={() => onScan(partition, mode)}>{t("读取目录")}</button>
        ) : (
          <button className="button primary" disabled={busy || !ready} onClick={() => files && onOpen({
            id: files.id,
            config: { layout, sector, stripe: striped ? stripe : 0 },
            options: { sector, kind },
            members: members.map(({ file, start, size }) => ({ file, range: { start: Number(start), size: Number(size) } })),
          })}>{t("重组并检查")}</button>
        )}
      </div>
    </Dialog>
  );
}

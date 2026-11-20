import { useState } from "react";
import { Dialog, FormError } from "./components";
import { useLocale } from "./i18n/LanguageProvider";
import { type ImageBrowseInfo, type ImageBrowseMode } from "./ImageBrowserPanel";
import { physicalMemberOptions, type LvmMember } from "./LvmBrowserDialog";
import type { LostCandidate } from "./lostPartitionModel";
import { formatSize, type Disk } from "./types";

export type RaidFiles = {
  id: string;
  files: { id: string; name: string; size: number }[];
};
export type RaidView = ImageBrowseInfo & {
  checkedRows: number;
  missingMembers: number;
  mdVersion: string | null;
  search: {
    id: string;
    state: "running" | "completed" | "canceled" | "failed";
    done: number;
    total: number;
    candidates: LostCandidate[];
    message: string;
  } | null;
};
export type RaidSearchRange = { startSector: number; sectorCount: number };
const layouts = [
  ["jbod", "JBOD"],
  ["raid0", "RAID 0"],
  ["raid0Original", "RAID 0 · Linux md original"],
  ["raid0Alternate", "RAID 0 · Linux md alternate"],
  ["raid1", "RAID 1"],
  ["raid10", "RAID 10 (near)"],
  ["raid10Md", "RAID 10 · Linux md"],
  ["raid10MdAdvanced", "RAID 10 · Linux md advanced"],
  ["raid4", "RAID 4"],
  ["raid4ParityLast", "RAID 4 · parity last"],
  ["raid5LeftAsymmetric", "RAID 5 · left asymmetric"],
  ["raid5RightAsymmetric", "RAID 5 · right asymmetric"],
  ["raid5LeftSymmetric", "RAID 5 · left symmetric"],
  ["raid5RightSymmetric", "RAID 5 · right symmetric"],
  ["raid5FixedFirst", "RAID 5 · parity first"],
  ["raid5FixedLast", "RAID 5 · parity last"],
  ["raid6LeftAsymmetric", "RAID 6 · left asymmetric"],
  ["raid6RightAsymmetric", "RAID 6 · right asymmetric"],
  ["raid6LeftSymmetric", "RAID 6 · left symmetric"],
  ["raid6RightSymmetric", "RAID 6 · right symmetric"],
  ["raid6FixedFirst", "RAID 6 · P/Q first"],
  ["raid6FixedLast", "RAID 6 · P/Q last"],
  ["raid6ZeroRestart", "RAID 6 · DDF zero restart"],
  ["raid6NRestart", "RAID 6 · DDF N restart"],
  ["raid6NContinue", "RAID 6 · DDF N continue"],
  ["raid6LeftAsymmetricQLast", "RAID 6 · left asymmetric, Q last"],
  ["raid6RightAsymmetricQLast", "RAID 6 · right asymmetric, Q last"],
  ["raid6LeftSymmetricQLast", "RAID 6 · left symmetric, Q last"],
  ["raid6RightSymmetricQLast", "RAID 6 · right symmetric, Q last"],
  ["raid6PFirstQLast", "RAID 6 · P first, Q last"],
] as const;
type LayoutChoice = (typeof layouts)[number][0];
type Raid10Layout = { format: "near" | "far" | "offset"; copies: number; farSets: boolean };
type Raid10Geometry = { nearCopies: number; farCopies: number; farOffset: boolean; groups: "all" | "legacy" | "fixed" };
type RaidLayout = Exclude<LayoutChoice, "raid10Md" | "raid10MdAdvanced"> |
  { raid10Md: Raid10Layout } | { raid10MdAdvanced: Raid10Geometry };
const defaultRaid10: Raid10Layout = { format: "near", copies: 2, farSets: false };
const defaultRaid10Advanced: Raid10Geometry = { nearCopies: 2, farCopies: 2, farOffset: false, groups: "all" };

function layoutChoice(layout: RaidLayout): LayoutChoice {
  if (typeof layout === "string") return layout;
  return "raid10Md" in layout ? "raid10Md" : "raid10MdAdvanced";
}
type Member = { key: string; file: string | null; start: string; size: string };
export type RaidRequest = {
  id: string;
  replaces?: string;
  config: { layout: RaidLayout; sector: number; stripe: number };
  options: { kind: "disk" | "partition"; sector: number };
  openWithoutTable: boolean;
  mdFingerprint?: string;
  members: { file: string | null; range: { start: number; size: number } }[];
};
export type RaidEdit = { sources: RaidFiles; request: RaidRequest & { replaces: string } };
export type RaidDetection = Pick<RaidRequest, "config" | "members"> & {
  version: string;
  fingerprint: string;
};

function memberRows(files: RaidFiles | null, initial?: Pick<RaidRequest, "id" | "members">): Member[] {
  if (initial && initial.id === files?.id) {
    return initial.members.map(({ file, range }, position) => ({
      key: file ?? `missing-${position}`,
      file,
      start: String(range.start),
      size: String(range.size),
    }));
  }
  return (files?.files ?? []).map((file) => ({
    key: file.id,
    file: file.id,
    start: "0",
    size: String(file.size),
  }));
}

export function VirtualRaidDialog({
  busy, pending, error, disks, files, view, initial, initialMdVersion, onSelect,
  onSelectPhysical, onDetectMd, onEdit,
  onOpen, onScan, onSearch, onStopSearch, onBrowseFound, onClose, onCloseRaid,
}: {
  busy: boolean;
  pending: boolean;
  error: string;
  disks: Disk[];
  files: RaidFiles | null;
  view: RaidView | null;
  initial?: RaidRequest;
  initialMdVersion?: string | null;
  onSelect: () => void;
  onSelectPhysical: (members: LvmMember[]) => void;
  onDetectMd: (
    request: { id: string; replaces?: string; sector: number },
    apply: (detected: RaidDetection) => void,
  ) => void;
  onEdit: () => void;
  onOpen: (request: RaidRequest) => void;
  onScan: (partition: number, mode: ImageBrowseMode) => void;
  onSearch: (range: RaidSearchRange) => void;
  onStopSearch: (id: string) => void;
  onBrowseFound: (candidate: string, mode: ImageBrowseMode) => void;
  onClose: () => void;
  onCloseRaid: () => void;
}) {
  const { t, catalogText, language } = useLocale();
  const initialLayout = initial?.config.layout ?? "raid0";
  const [layout, setLayout] = useState(layoutChoice(initialLayout));
  const [raid10, setRaid10] = useState(typeof initialLayout !== "string" && "raid10Md" in initialLayout
    ? initialLayout.raid10Md : defaultRaid10);
  const [advanced, setAdvanced] = useState(typeof initialLayout !== "string" && "raid10MdAdvanced" in initialLayout
    ? initialLayout.raid10MdAdvanced : defaultRaid10Advanced);
  const [sector, setSector] = useState(initial?.config.sector ?? 512);
  const [stripe, setStripe] = useState(initial?.config.stripe || 64 * 1024);
  const [kind, setKind] = useState<"disk" | "partition" | "search">(
    initial?.openWithoutTable ? "search" : (initial?.options.kind ?? "disk"),
  );
  const [devicePickerGroup, setDevicePickerGroup] = useState<string | null>(null);
  const selectingDevices = devicePickerGroup === (files?.id ?? "");
  const [selectedDevices, setSelectedDevices] = useState<string[]>([]);
  const [deviceFilter, setDeviceFilter] = useState("");
  const [startSector, setStartSector] = useState("0");
  const [sectorCount, setSectorCount] = useState(
    view ? String(view.size / view.options.sector) : "",
  );
  const [memberDraft, setMemberDraft] = useState(() => ({
    id: files?.id,
    rows: memberRows(files, initial),
  }));
  const [md, setMd] = useState(() => initial?.mdFingerprint && initialMdVersion ? {
    id: initial.id, fingerprint: initial.mdFingerprint, version: initialMdVersion,
  } : null);
  const appliedMd = md?.id === files?.id ? md : null;
  const members = memberDraft.id === files?.id ? memberDraft.rows : memberRows(files, initial);
  const setMembers = (rows: Member[]) => {
    setMd(null);
    setMemberDraft({ id: files?.id, rows });
  };
  const detect = () => {
    if (!files) return;
    onDetectMd({ id: files.id, replaces: initial?.replaces, sector }, (detected) => {
      const detectedLayout = detected.config.layout;
      setLayout(layoutChoice(detectedLayout));
      if (typeof detectedLayout !== "string") {
        if ("raid10Md" in detectedLayout) setRaid10(detectedLayout.raid10Md);
        else setAdvanced(detectedLayout.raid10MdAdvanced);
      }
      setSector(detected.config.sector);
      setStripe(detected.config.stripe || 64 * 1024);
      setMemberDraft({ id: files.id, rows: memberRows(files, { id: files.id, members: detected.members }) });
      setMd({ id: files.id, fingerprint: detected.fingerprint, version: detected.version });
    });
  };
  const [partition, setPartition] = useState(view?.partitions[0]?.id ?? 0);
  const [mode, setMode] = useState<ImageBrowseMode>("existing");
  const offered = selectingDevices ? physicalMemberOptions(disks, t) : [];
  const selected = offered.filter(({ key }) => selectedDevices.includes(key));
  const query = deviceFilter.trim().toLocaleLowerCase();
  const searchRange = { startSector: Number(startSector), sectorCount: Number(sectorCount) };
  const searchReady = !!view &&
    /^\d+$/.test(startSector) && /^\d+$/.test(sectorCount) &&
    [searchRange.startSector, searchRange.sectorCount].every(Number.isSafeInteger) &&
    searchRange.sectorCount > 0 &&
    searchRange.startSector + searchRange.sectorCount <= view.size / view.options.sector;
  const striped = layout !== "jbod" && layout !== "raid1";
  const advancedCopies = advanced.nearCopies * advanced.farCopies;
  const validAdvanced = [advanced.nearCopies, advanced.farCopies].every((copies) => Number.isInteger(copies) && copies >= 1) &&
    advancedCopies >= 2 && advancedCopies <= members.length;
  const ready = !!files && members.length >= 2 &&
    (layout !== "raid10Md" || (Number.isInteger(raid10.copies) && raid10.copies >= 2 && raid10.copies <= members.length)) &&
    (layout !== "raid10MdAdvanced" || validAdvanced) &&
    members.every(({ start, size }) => /^\d+$/.test(start) && /^\d+$/.test(size) &&
      Number.isSafeInteger(Number(start)) && Number.isSafeInteger(Number(size)) && Number(size) > 0);
  const changeRaid10 = (patch: Partial<Raid10Layout>) => {
    setMd(null);
    setRaid10({ ...raid10, ...patch });
  };
  const changeAdvanced = (patch: Partial<Raid10Geometry>) => {
    setMd(null);
    setAdvanced({ ...advanced, ...patch });
  };
  const requestLayout: RaidLayout = layout === "raid10Md" ? { raid10Md: raid10 }
    : layout === "raid10MdAdvanced" ? { raid10MdAdvanced: advanced } : layout;
  const change = (index: number, patch: Partial<Member>) =>
    setMembers(members.map((row, number) => number === index ? { ...row, ...patch } : row));
  const move = (index: number, delta: number) => {
    const next = [...members];
    [next[index], next[index + delta]] = [next[index + delta], next[index]];
    setMembers(next);
  };
  return (
    <Dialog title={t(initial ? "重新组装虚拟 RAID" : "虚拟 RAID")} wide onClose={() => !busy && onClose()}>
      {view ? (
        <>
          <p>{view.name} · {formatSize(view.size)} · {t("{size} B 扇区", { size: view.options.sector })}</p>
          {view.mdVersion && <p className="lost-note">{t("Linux md {version} 元数据", { version: view.mdVersion })}</p>}
          {view.checkedRows > 0 && <p className="lost-note">{t("通过 {count} 个条带校验样本。", { count: view.checkedRows })}</p>}
          {view.checkedRows > 0 && <p className="lost-note">{t("条带校验无法单独确认成员顺序或布局。")}</p>}
          {view.missingMembers > 0 && <p className="lost-note">{t("缺少 {count} 个 RAID 成员。", { count: view.missingMembers })}</p>}
          {view.warning && <p className="lost-note">{catalogText(view.warning)}</p>}
          {view.partitions.length > 0 && <label className="backup-browser-partition">
            {t("镜像中的分区")}
            <select disabled={busy} value={partition} onChange={(e) => setPartition(Number(e.target.value))}>
              {view.partitions.map((part) => <option key={part.id} value={part.id}>{catalogText(part.name)} · {formatSize(part.size)}</option>)}
            </select>
          </label>}
          <label className="backup-browser-partition">
            {t("文件范围")}
            <select value={mode} disabled={busy} onChange={(e) => setMode(e.target.value as ImageBrowseMode)}>
              <option value="existing">{t("现存文件")}</option>
              <option value="allFiles">{t("全部文件")}</option>
              <option value="deleted">{t("找回删除文件")}</option>
            </select>
          </label>
          {view.options.kind === "disk" && (
            <section className="lost-results">
              <h3>{t("搜索丢失分区")}</h3>
              <div className="image-browser-open-row">
                <label>{t("起始 LBA")}
                  <input value={startSector} disabled={busy} inputMode="numeric" onChange={(event) => setStartSector(event.target.value)} />
                </label>
                <label>{t("扇区数量")}
                  <input value={sectorCount} disabled={busy} inputMode="numeric" onChange={(event) => setSectorCount(event.target.value)} />
                </label>
              </div>
              {view.search?.state === "running" ? (
                <button className="button" disabled={pending} onClick={() => onStopSearch(view.search!.id)}>{t("停止搜索")}</button>
              ) : (
                <button className="button" disabled={busy || !searchReady} onClick={() => onSearch(searchRange)}>
                  {view.search ? t("重新搜索") : t("开始搜索")}
                </button>
              )}
              {view.search && (
                <div aria-live="polite">
                  <div className="lost-progress">
                    <span>{t("找到 {count} 个候选", { count: view.search.candidates.length.toLocaleString(language) })}</span>
                    <span>{formatSize(view.search.done)} / {formatSize(view.search.total)}</span>
                  </div>
                  <progress value={view.search.done} max={view.search.total} />
                  {view.search.message && <p className="lost-note">{catalogText(view.search.message)}</p>}
                  {view.search.candidates.map((candidate) => (
                    <article className="lost-candidate" key={candidate.id}>
                      <div>
                        <strong>{candidate.filesystem} · {formatSize(candidate.size)}</strong>
                        <small>{t("起始 LBA")} {(candidate.offset / view.options.sector).toLocaleString(language)}</small>
                        {candidate.evidence && <small>{catalogText(candidate.evidence)}</small>}
                        {candidate.warning && <small>{catalogText(candidate.warning)}</small>}
                      </div>
                      {candidate.canBrowse && <button className="button" disabled={busy} onClick={() => onBrowseFound(candidate.id, mode)}>{t("读取目录")}</button>}
                    </article>
                  ))}
                </div>
              )}
            </section>
          )}
        </>
      ) : (
        <>
          {initial && <p className="lost-note">{t("应用参数后请重新搜索或读取目录。")}</p>}
          <p className="lost-note">{t("按阵列顺序排列成员，填写每个成员的数据偏移和长度。虚拟 RAID 使用只读来源。")}</p>
          <div className="image-browser-open-row">
            <button className="button" disabled={busy} onClick={onSelect}>{t("选择 RAID 成员镜像")}</button>
            <button className="button" disabled={busy} onClick={() => {
              setDeviceFilter("");
              setDevicePickerGroup(selectingDevices ? null : (files?.id ?? ""));
            }}>{t("选择物理磁盘或分区")}</button>
          </div>
          {selectingDevices && (
            <section className="lost-results">
              {offered.length > 8 && <label className="backup-browser-partition">
                {t("筛选磁盘或分区")}
                <input type="search" value={deviceFilter} disabled={busy} onChange={(event) => setDeviceFilter(event.target.value)} />
              </label>}
              <div className="btrfs-member-options">
                {offered.filter(({ label }) => label.toLocaleLowerCase().includes(query)).map(({ key, source, label }) => (
                  <label key={key}>
                    <input
                      type="checkbox"
                      checked={selectedDevices.includes(key)}
                      disabled={busy}
                      onChange={(event) => {
                        const keep = selected.filter((item) => item.key !== key && (
                          item.source.target !== source.target ||
                          (item.source.partition !== null && source.partition !== null)
                        ));
                        setSelectedDevices(event.target.checked
                          ? [...keep.map((item) => item.key), key]
                          : selectedDevices.filter((item) => item !== key));
                      }}
                    />
                    <span>{label}</span>
                  </label>
                ))}
              </div>
              <button className="button" disabled={busy || !selected.length || selected.length > 64}
                onClick={() => onSelectPhysical(selected.map(({ source }) => source))}>
                {t("使用所选物理成员")}
              </button>
            </section>
          )}
          {files && <>
            <div className="image-browser-open-row">
              <label>{t("RAID 布局")}<select value={layout} disabled={busy} onChange={(e) => {
                setMd(null);
                setLayout(e.target.value as typeof layout);
              }}>
                {layouts.map(([id, name]) => <option key={id} value={id}>{name}</option>)}
              </select></label>
              <label>{t("RAW 扇区")}<select value={sector} disabled={busy} onChange={(e) => {
                const value = Number(e.target.value);
                setMd(null);
                setSector(value);
                setStripe((previous) => Math.max(previous, value));
              }}>
                <option value={512}>512 B</option><option value={4096}>4096 B</option>
              </select></label>
              {striped && <label>{t("条带大小")}<select value={stripe} disabled={busy} onChange={(e) => {
                setMd(null);
                setStripe(Number(e.target.value));
              }}>
                {[512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576].filter((size) => size >= sector)
                  .map((size) => <option key={size} value={size}>{formatSize(size)}</option>)}
              </select></label>}
              <label>{t("镜像范围")}<select value={kind} disabled={busy} onChange={(e) => setKind(e.target.value as typeof kind)}>
                <option value="disk">{t("整盘镜像（GPT / MBR）")}</option>
                <option value="partition">{t("分区镜像（文件系统）")}</option>
                <option value="search">{t("整盘搜索（分区表缺失或损坏）")}</option>
              </select></label>
            </div>
            {layout === "raid10Md" && <div className="image-browser-open-row">
              <label>{t("RAID10 副本布局")}
                <select value={raid10.format} disabled={busy} onChange={(e) => {
                  const format = e.target.value as Raid10Layout["format"];
                  changeRaid10({ format, farSets: format !== "near" && raid10.farSets });
                }}>
                  <option value="near">near</option>
                  <option value="far">far</option>
                  <option value="offset">offset</option>
                </select>
              </label>
              <label>{t("RAID10 副本数")}
                <input type="number" min={2} max={members.length} value={raid10.copies || ""} disabled={busy}
                  onChange={(e) => changeRaid10({ copies: Number(e.target.value) })} />
              </label>
              {raid10.format !== "near" && <label className="virtual-raid-check">
                <input type="checkbox" checked={raid10.farSets} disabled={busy}
                  onChange={(e) => changeRaid10({ farSets: e.target.checked })} />
                {t("在组内轮转远端副本")}
              </label>}
            </div>}
            {layout === "raid10MdAdvanced" && <div className="image-browser-open-row">
              <label>{t("近端副本数")}
                <input type="number" min={1} max={members.length} value={advanced.nearCopies || ""} disabled={busy}
                  onChange={(e) => changeAdvanced({ nearCopies: Number(e.target.value) })} />
              </label>
              <label>{t("远端副本数")}
                <input type="number" min={1} max={members.length} value={advanced.farCopies || ""} disabled={busy}
                  onChange={(e) => changeAdvanced({ farCopies: Number(e.target.value) })} />
              </label>
              <label>{t("远端副本分组")}
                <select value={advanced.groups} disabled={busy}
                  onChange={(e) => changeAdvanced({ groups: e.target.value as Raid10Geometry["groups"] })}>
                  <option value="all">{t("跨全部成员")}</option>
                  <option value="legacy">{t("旧式分组")}</option>
                  <option value="fixed">{t("固定分组")}</option>
                </select>
              </label>
              <label className="virtual-raid-check">
                <input type="checkbox" checked={advanced.farOffset} disabled={busy}
                  onChange={(e) => changeAdvanced({ farOffset: e.target.checked })} />
                {t("使用 offset 布局")}
              </label>
            </div>}
            <div className="image-browser-open-row">
              <button className="button" disabled={busy} onClick={detect}>{t("识别 Linux md 参数")}</button>
              {appliedMd && <span role="status">{t("Linux md {version} 元数据", { version: appliedMd.version })}</span>}
            </div>
            {!appliedMd && <p className="lost-note">{t("识别前请选择整个 RAID 成员范围及阵列的逻辑扇区大小。")}</p>}
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
        {view && <button className="button" disabled={busy} onClick={onEdit}>{t("修改阵列参数")}</button>}
        {view && <button className="button" disabled={busy} onClick={onCloseRaid}>{t("关闭阵列")}</button>}
        <button className="button" disabled={busy} onClick={onClose}>{t("取消")}</button>
        {view ? (view.partitions.length > 0 &&
          <button className="button primary" disabled={busy || !view.partitions.length} onClick={() => onScan(partition, mode)}>{t("读取目录")}</button>
        ) : (
          <button className="button primary" disabled={busy || !ready} onClick={() => files && onOpen({
            id: files.id,
            replaces: initial?.replaces,
            config: { layout: requestLayout, sector, stripe: striped ? stripe : 0 },
            options: { sector, kind: kind === "search" ? "disk" : kind },
            openWithoutTable: kind === "search",
            mdFingerprint: appliedMd?.fingerprint,
            members: members.map(({ file, start, size }) => ({ file, range: { start: Number(start), size: Number(size) } })),
          })}>{t("重组并检查")}</button>
        )}
      </div>
    </Dialog>
  );
}

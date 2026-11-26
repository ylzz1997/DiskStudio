import { useLocale } from "./i18n/LanguageProvider";
import { formatSize, type Disk, type MigrationLayout, type MigrationPartitionPlan, type MigrationPartitionSize, type MigrationReplacedPartition, type MigrationSelection } from "./types";

const MiB = 1024 ** 2;

export type PartitionSize = MigrationPartitionSize;
export type { MigrationSelection };
export const defaultMigrationSelection: MigrationSelection = { mode: "selected", partitions: [] };
type PartitionFields = { size?: string; start?: string };
export type MigrationValues = Record<number, PartitionFields>;

export type MigrationTarget = {
  id: string;
  values: MigrationValues;
  selection: MigrationSelection;
};

export function migrationOptions(disk: Disk | undefined, target: MigrationTarget) {
  const partitionSelection = migrationSelection(disk, target.selection);
  return { partitionSelection, partitionSizes: migrationSizes(disk, target.values, partitionSelection) };
}

export function canReplaceMigrationTarget(source: Disk, target: Disk): boolean {
  const hardware = target.hardware;
  return target.kind === "physical" && target.id !== source.id &&
    !!hardware?.identity && !!hardware.layoutKey && hardware.layoutAvailable &&
    !hardware.isSystem && hardware.deviceReadOnly === false && typeof hardware.offline === "boolean" &&
    target.sectorSize === source.sectorSize && (source.scheme !== "MBR" || target.size >= source.size) &&
    (target.scheme === "RAW" || target.scheme === "GPT") &&
    target.partitions.every((part) => part.number === undefined ||
      (target.scheme === "GPT" && part.role === "数据分区" && !part.system && !part.hidden));
}

export function MigrationTargets({ source, choices, targets, onChange, disabled, hot = false }: {
  source: Disk;
  choices: { id: string; label: string }[];
  targets: MigrationTarget[];
  onChange: (targets: MigrationTarget[]) => void;
  disabled: boolean;
  hot?: boolean;
}) {
  const { t } = useLocale();
  const available = choices.filter((choice) => !targets.some((target) => target.id === choice.id));
  function patch(id: string, update: Partial<MigrationTarget>) {
    onChange(targets.map((target) => target.id === id ? { ...target, ...update } : target));
  }
  return (
    <div className="migration-targets">
      <label>
        {t("添加目标磁盘")}
        <select disabled={disabled || !available.length || targets.length >= 8} value=""
          onChange={(event) => {
            const id = event.currentTarget.value;
            if (id) onChange([...targets, { id, values: {}, selection: defaultMigrationSelection }]);
          }}>
          <option value="">{t("选择另一块数据磁盘")}</option>
          {available.map(({ id, label }) => <option key={id} value={id}>{label}</option>)}
        </select>
      </label>
      {targets.map((target) => {
        const choice = choices.find(({ id }) => id === target.id);
        const options = migrationOptions(source, target);
        return (
          <div className="migration-target" key={target.id}>
            <div className="migration-target-header">
              <strong>{choice?.label ?? t("目标磁盘已不可用")}</strong>
              <button type="button" className="button" disabled={disabled}
                aria-label={t("移除目标磁盘 {disk}", { disk: choice?.label ?? target.id })}
                onClick={() => onChange(targets.filter(({ id }) => id !== target.id))}>{t("移除")}</button>
            </div>
            {choice && source.scheme === "GPT" && (
              <details open={targets.length === 1}>
                <summary>{t("迁移分区和布局")}{invalidMigrationSizes(options.partitionSizes) &&
                  ` · ${t("请检查起点和容量")}`}</summary>
                <MigrationSizes disk={source} values={target.values} selection={options.partitionSelection} hot={hot}
                  onChange={(values) => patch(target.id, { values })}
                  onSelectionChange={(selection) => patch(target.id, { selection })} disabled={disabled} />
              </details>
            )}
          </div>
        );
      })}
    </div>
  );
}

function optionalPartitions(disk?: Disk) {
  return disk?.scheme === "GPT" ? disk.partitions.filter((part) => part.number !== undefined && (
    part.role === "Linux 数据" || part.role === "Linux 交换" ||
    (!part.hidden && (part.role === "数据分区" || part.role === "系统分区"))
  )) : [];
}

export function migrationSelection(disk: Disk | undefined, selection: MigrationSelection): MigrationSelection {
  if (disk?.scheme !== "GPT" || selection.mode === "all") return { mode: "all" };
  const available = optionalPartitions(disk);
  return { mode: "selected", partitions: selection.partitions.filter((number) =>
    available.some((part) => part.number === number)) };
}

function eligiblePartitions(disk: Disk | undefined, selection: MigrationSelection) {
  return disk?.scheme === "GPT" ? disk.partitions.filter((p) =>
    p.number !== undefined && !p.hidden &&
    (p.role === "数据分区" || p.role === "Windows 系统") &&
    (selection.mode === "all" || p.role === "Windows 系统" || selection.partitions.includes(p.number)) &&
    (p.fs.toUpperCase() === "NTFS" || (disk.hardware?.offline === true && p.fs === "未知"))) : [];
}

export function migrationSizes(disk: Disk | undefined, values: MigrationValues, selection: MigrationSelection): PartitionSize[] {
  return eligiblePartitions(disk, selection).flatMap((part) => {
    const partition = part.number!;
    const size = values[partition]?.size?.trim();
    const start = values[partition]?.start?.trim();
    return size || start ? [{
      partition,
      ...(size && { bytes: Number(size) * MiB }),
      ...(start && { startBytes: Number(start) * MiB }),
    }] : [];
  });
}

export function invalidMigrationSizes(sizes: PartitionSize[]) {
  return sizes.some(({ bytes, startBytes }) =>
    (bytes !== undefined && (!Number.isSafeInteger(bytes) || bytes < 16 * MiB ||
      bytes > 2 ** 43 || bytes % MiB !== 0)) ||
    (startBytes !== undefined && (!Number.isSafeInteger(startBytes) || startBytes < MiB ||
      startBytes >= 2 ** 43 || startBytes % MiB !== 0)));
}

function MigrationSizes({ disk, values, selection, onChange, onSelectionChange, disabled, hot }: {
  disk?: Disk;
  values: MigrationValues;
  selection: MigrationSelection;
  onChange: (next: MigrationValues) => void;
  onSelectionChange: (next: MigrationSelection) => void;
  disabled: boolean;
  hot: boolean;
}) {
  const { t } = useLocale();
  if (disk?.scheme !== "GPT") return null;
  const parts = eligiblePartitions(disk, selection);
  const available = optionalPartitions(disk);
  const optional = hot ? available.filter((part) => part.fs.toUpperCase() === "NTFS" && !part.encryption) : available;
  function update(number: number, field: keyof PartitionFields, value: string) {
    onChange({ ...values, [number]: { ...values[number], [field]: value } });
  }
  return (
    <>
    <fieldset className="migration-selection" disabled={disabled}>
      <legend>{t("迁移分区")}</legend>
      <label>
        {t("迁移范围")}
        <select value={selection.mode} onChange={(event) =>
          onSelectionChange(event.currentTarget.value === "all" ? { mode: "all" } : defaultMigrationSelection)}>
          <option value="selected">{t("Windows 必需分区和所选数据分区")}</option>
          <option value="all" disabled={hot && optional.length !== available.length}>{t("复制全部分区")}</option>
        </select>
      </label>
      {selection.mode === "selected" && optional.map((part) => {
        const number = part.number!;
        return (
          <label className="migration-partition-option" key={part.id}>
            <input type="checkbox" checked={selection.partitions.includes(number)}
              onChange={(event) => onSelectionChange({
                mode: "selected",
                partitions: event.currentTarget.checked ? [...selection.partitions, number] : selection.partitions.filter((value) => value !== number),
              })} />
            <span>{t("分区 {number}", { number })} · {part.name} · {formatSize(part.size)}</span>
          </label>
        );
      })}
      {selection.mode === "selected" &&
        <small>{t("启动、恢复和受保护分区自动保留。要为其他分区调整起点或容量，请先勾选它。")}</small>}
    </fieldset>
    {parts.length > 0 && (
    <fieldset className="disk-clone-sizes migration-sizes" disabled={disabled}>
      <legend>{t("目标 NTFS 分区布局（MiB）")}</legend>
      {parts.map((part) => (
        <fieldset className="migration-partition-fields" key={part.id}>
          <legend>{t("分区 {number}", { number: part.number! })} · {part.name}</legend>
          <label>
            {t("起点（MiB）")}
            <input type="number" min={1} step={1} placeholder={String(part.start / MiB)}
              value={values[part.number!]?.start ?? ""}
              onChange={(event) => update(part.number!, "start", event.currentTarget.value)} />
          </label>
          <label>
            {t("容量（MiB）")}
            <input type="number" min={16} step={1} placeholder={String(part.size / MiB)}
              value={values[part.number!]?.size ?? ""}
              onChange={(event) => update(part.number!, "size", event.currentTarget.value)} />
          </label>
        </fieldset>
      ))}
      <small>{t("留空即沿用源分区的起点或容量。首次启动时 Windows 会检查调整过容量的 NTFS 卷。")}</small>
    </fieldset>
    )}
    </>
  );
}

export function MigrationPartitionPreview({ selection }: { selection: MigrationPartitionPlan }) {
  const { t } = useLocale();
  return (
    <>
      <dt>{t("复制到目标盘")}</dt>
      <dd>
        <ul className="migration-preview-list">
          {selection.copied.map((part) => <li key={part.partition}>
            {t("分区 {number}", { number: part.partition })} · {formatSize(part.bytes)} · {(part.offset / MiB).toLocaleString()} MiB
            {part.required && ` · ${t("自动保留")}`}
          </li>)}
        </ul>
      </dd>
      {selection.skipped.length > 0 && <>
        <dt>{t("跳过的分区")}</dt>
        <dd>
          <ul className="migration-preview-list">
            {selection.skipped.map((part) =>
              <li key={part.partition}>{t("分区 {number}", { number: part.partition })} · {formatSize(part.bytes)}</li>)}
          </ul>
        </dd>
      </>}
    </>
  );
}

export function ReplacedMigrationPartitions({ partitions }: { partitions?: MigrationReplacedPartition[] }) {
  const { t } = useLocale();
  if (!partitions?.length) return null;
  return <>
    <dt>{t("将被覆盖的现有分区")}</dt>
    <dd>
      <ul className="migration-preview-list">
        {partitions.map((part) => <li key={part.number}>
          {t("分区 {number}", { number: part.number })} · {formatSize(part.bytes)} · {(part.offset / MiB).toLocaleString()} MiB
        </li>)}
      </ul>
    </dd>
  </>;
}

export function MigrationLayoutPreview({ layout }: { layout: MigrationLayout }) {
  const { t } = useLocale();
  return (
    <>
      <dt>{t("启动固件")}</dt>
      <dd>{layout.firmware.toUpperCase()} · {layout.sector.toLocaleString()} B</dd>
      <dt>{t("整盘容量")}</dt>
      <dd>{formatSize(layout.sourceBytes)} → {formatSize(layout.targetBytes)}</dd>
      <dt>{t("迁移分区")}</dt>
      <dd>
        {layout.partitions.toLocaleString()}
        {layout.relocatedGpt && ` · ${t("迁移备份 GPT 到目标盘末尾")}`}
      </dd>
      <dt>{t("启动分区起点")}</dt>
      <dd>{layout.bootOffset.toLocaleString()} B</dd>
      <dt>{t("Windows 分区起点")}</dt>
      <dd>{layout.windowsOffset.toLocaleString()} B</dd>
      {layout.partitionSelection && <MigrationPartitionPreview selection={layout.partitionSelection} />}
      {layout.partitionChanges.map((part) => (
        <div key={part.partition}>
          <dt>{t("分区 {number}", { number: part.partition })}</dt>
          {part.offset !== part.newOffset && (
            <dd>{t("起点（MiB）")} · {(part.offset / MiB).toLocaleString()} → {(part.newOffset / MiB).toLocaleString()}</dd>
          )}
          {part.oldBytes !== part.newBytes && <dd>{formatSize(part.oldBytes)} → {formatSize(part.newBytes)}</dd>}
        </div>
      ))}
    </>
  );
}

export function MigrationRequestDetails({ selection, sizes }: {
  selection: MigrationSelection;
  sizes: PartitionSize[];
}) {
  const { t } = useLocale();
  return <>
    <dt>{t("迁移范围")}</dt>
    <dd>{t(selection.mode === "selected" ? "Windows 必需分区和所选数据分区" : "复制全部分区")}</dd>
    {selection.mode === "selected" && selection.partitions.map((number) =>
      <dd key={number}>{t("分区 {number}", { number })}</dd>)}
    {sizes.map((part) => (
      <div key={part.partition}>
        <dt>{t("分区 {number}", { number: part.partition })}</dt>
        {part.startBytes !== undefined && <dd>{t("起点（MiB）")} · {(part.startBytes / MiB).toLocaleString()}</dd>}
        {part.bytes !== undefined && <dd>{formatSize(part.bytes)}</dd>}
      </div>
    ))}
  </>;
}

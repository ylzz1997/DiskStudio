export type RecoveryLayout = "tree" | "flat";
export type ExportOptions = {
  timestamps: boolean;
  attributes: boolean;
  workers: 1 | 2 | 4;
};
export const defaultExportOptions: ExportOptions = {
  timestamps: true,
  attributes: true,
  workers: 2,
};
export type FileStamp = { text: string; zone: string; precision: string };
export type FileMetadata = {
  source: string | null;
  created: FileStamp | null;
  modified: FileStamp | null;
  accessed: FileStamp | null;
  changed: FileStamp | null;
  attributes: string[];
  rawFlags: string | null;
  permissions: string | null;
  uid: number | null;
  gid: number | null;
  links: number | null;
};
export type Candidate = {
  id: number;
  name: string;
  size: number;
  deleted: boolean;
  method: string;
  note: string;
  supported: boolean;
  directory: string | null;
  directoryNote: string;
  metadata?: FileMetadata;
};
export type RecoveryFilter = {
  folder: string | null;
  descendants: boolean;
  state: "all" | "deleted" | "existing" | "unknown";
  kind:
    "all" | "documents" | "images" | "videos" | "audio" | "archives" | "other";
  minSize: number | null;
  maxSize: number | null;
  supportedOnly: boolean;
  nameMatch: "contains" | "exact" | "prefix" | "extension";
  caseSensitive: boolean;
  sortBy: "scan" | "name" | "size" | "extension" | "modified";
  sortDirection: "asc" | "desc";
  dateField: "created" | "modified" | "accessed" | "changed";
  dateFrom: string | null;
  dateTo: string | null;
};
export type RecoveryResultsPage = {
  scanId: string;
  source: string;
  total: number;
  scannedTotal: number;
  offset: number;
  items: Candidate[];
  paths: Record<string, string[]>;
};
export type RecoveryDirectory = {
  id: string;
  parent: string | null;
  name: string;
  deleted: boolean;
  metadata?: FileMetadata;
};
export type RecoveryFolders = {
  canExportDirectory: boolean;
  path: RecoveryDirectory[];
  items: (RecoveryDirectory & { files: number })[];
  total: number;
  offset: number;
  unknownFiles: number;
};
export const initialRecoveryFilter: RecoveryFilter = {
  folder: null,
  descendants: true,
  state: "all",
  kind: "all",
  minSize: null,
  maxSize: null,
  supportedOnly: false,
  nameMatch: "contains",
  caseSensitive: false,
  sortBy: "scan",
  sortDirection: "asc",
  dateField: "modified",
  dateFrom: null,
  dateTo: null,
};
export function dateFilter(
  from: string,
  to: string,
): Pick<RecoveryFilter, "dateFrom" | "dateTo"> {
  function parse(value: string) {
    if (!value) return null;
    if (!/^\d{4}-\d{2}-\d{2}$/.test(value))
      throw Error("请输入有效的 YYYY-MM-DD 日期。");
    const [year, month, day] = value.split("-").map(Number);
    const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
    const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if (year < 1 || month < 1 || month > 12 || day < 1 || day > days[month - 1])
      throw Error("请输入有效的 YYYY-MM-DD 日期。");
    return value;
  }
  const dateFrom = parse(from),
    dateTo = parse(to);
  if (dateFrom && dateTo && dateFrom > dateTo)
    throw Error("开始日期不能晚于结束日期。");
  return { dateFrom, dateTo };
}
export function fileState(c: Candidate): string {
  return c.method === "文件签名"
    ? "删除状态未知"
    : c.deleted
      ? "已删除"
      : "现存文件";
}
export function selectedFiles(
  old: Set<number>,
  ids: number[],
  select: boolean,
): Set<number> {
  const next = new Set(old);
  ids.forEach((id) => (select ? next.add(id) : next.delete(id)));
  if (next.size > 10000)
    throw Error("每次最多选择 10000 个文件，请先缩小范围或清空选择。");
  return next;
}
export function sizeFilter(
  min: string,
  max: string,
): Pick<RecoveryFilter, "minSize" | "maxSize"> {
  function parse(value: string) {
    if (!value.trim()) return null;
    if (!/^\d+(\.\d+)?$/.test(value)) throw Error("请输入有效的 MiB 容量。");
    const bytes = Math.round(Number(value) * 1024 ** 2);
    if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > 2 ** 43)
      throw Error("容量筛选范围为 0 至 8 TiB。");
    return bytes;
  }
  const minSize = parse(min),
    maxSize = parse(max);
  if (minSize !== null && maxSize !== null && minSize > maxSize)
    throw Error("最小容量不能大于最大容量。");
  return { minSize, maxSize };
}

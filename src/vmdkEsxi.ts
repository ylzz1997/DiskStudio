// VMware Virtual Disk Format 5.0 (COWD) and the QEMU SEsparse layout.
import { logicalLimit, type ImageFile, type LogicalImage } from "./imageSource";
import {
  chargeVmdkTables,
  vmdkFileReads,
  vmdkRange,
  type VmdkBudget,
} from "./vmdkImage";

const SECTOR = 512;
const MiB = 1024 ** 2;
const TABLE_ENTRIES = 4096;
const INHERIT = 0;
const ZERO = -1;
type Kind = "VMFSSPARSE" | "SESPARSE";
type Span = { at: number; end: number };
type Layout = {
  grain: number;
  directory: Span;
  tableLength: number;
  metadata: Span[];
  tableLocation(dir: DataView, index: number): number;
  dataLocation(table: DataView, index: number): number;
};
type Reads = ReturnType<typeof vmdkFileReads>;

const view = (bytes: Uint8Array) =>
  new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
function sectorSpan(at: bigint, length: bigint): Span {
  const end = (at + length) * BigInt(SECTOR);
  if (end > BigInt(Number.MAX_SAFE_INTEGER))
    throw Error("ESXi VMDK 区段超出安全范围。");
  return { at: Number(at) * SECTOR, end: Number(end) };
}

function cowd(bytes: Uint8Array, size: number, fileSize: number): Layout {
  const header = view(bytes);
  const grain = header.getUint32(16, true) * SECTOR;
  const entries = header.getUint32(24, true);
  const at = header.getUint32(20, true) * SECTOR;
  const allocatedEnd = header.getUint32(28, true) * SECTOR;
  if (
    header.getUint32(0, true) !== 0x44574f43 ||
    header.getUint32(4, true) !== 1 ||
    header.getUint32(8, true) !== 3 ||
    header.getUint32(12, true) * SECTOR !== size ||
    grain < SECTOR ||
    grain > MiB ||
    (grain & (grain - 1)) !== 0 ||
    entries !== Math.ceil(size / (grain * TABLE_ENTRIES)) ||
    at < bytes.length ||
    allocatedEnd > fileSize
  )
    throw Error("VMFSSPARSE 头部、容量或粒度目录参数无效。");
  if (header.getUint32(1648, true) !== 0)
    throw Error("VMFSSPARSE 未正常关闭，请先在 VMware 中核对并关闭镜像。");
  const length = Math.ceil((entries * 4) / SECTOR) * SECTOR;
  vmdkRange(at, length, allocatedEnd);
  const directory = { at, end: at + length };
  const tableLength = TABLE_ENTRIES * 4;
  return {
    grain,
    directory,
    tableLength,
    metadata: [{ at: 0, end: bytes.length }, directory],
    tableLocation(dir, index) {
      const offset = dir.getUint32(index * 4, true) * SECTOR;
      if (offset) vmdkRange(offset, tableLength, allocatedEnd);
      return offset;
    },
    dataLocation(table, index) {
      const offset = table.getUint32(index * 4, true) * SECTOR;
      if (offset) vmdkRange(offset, grain, allocatedEnd);
      return offset;
    },
  };
}

async function sesparse(
  reader: Reads,
  size: number,
  fileSize: number,
): Promise<Layout> {
  const bytes = await reader.capture(0, SECTOR);
  const header = view(bytes);
  const field = (index: number) => header.getBigUint64(index * 8, true);
  if (
    field(0) !== 0xcafebaben ||
    field(1) !== 0x0000000200000001n ||
    field(2) * BigInt(SECTOR) !== BigInt(size) ||
    field(3) !== 8n ||
    field(4) !== 64n ||
    field(5) !== 0n ||
    (field(6) !== 0n && field(6) !== 512n) ||
    bytes.subarray(56, 80).some(Boolean) ||
    bytes.subarray(208).some(Boolean)
  )
    throw Error("SESPARSE 头部、容量或版本不受支持。");

  const reserved = Array.from({ length: 7 }, (_, i) =>
    sectorSpan(field(10 + i * 2), field(11 + i * 2)),
  );
  const [volatile, , , directory, tables] = reserved;
  const payload = sectorSpan(field(24), field(25));
  const grain = 4096;
  const tableLength = TABLE_ENTRIES * 8;
  const directoryEntries = Math.ceil(size / (grain * TABLE_ENTRIES));
  if (
    volatile.end - volatile.at < SECTOR ||
    directory.end - directory.at < directoryEntries * 8 ||
    (tables.end - tables.at) % tableLength ||
    (payload.end - payload.at) % grain ||
    payload.at > fileSize
  )
    throw Error("SESPARSE 目录、粒度表或数据区段长度无效。");
  const metadata = [{ at: 0, end: SECTOR }, ...reserved];
  let end = 0;
  for (const region of [...metadata, payload].sort((a, b) => a.at - b.at)) {
    if (region.at < end) throw Error("SESPARSE 容器区段重叠。");
    end = region.end;
  }
  for (const region of reserved)
    vmdkRange(region.at, region.end - region.at, fileSize);
  const stateBytes = await reader.capture(volatile.at, SECTOR);
  const state = view(stateBytes);
  if (
    state.getBigUint64(0, true) !== 0xcafecafen ||
    stateBytes.subarray(32).some(Boolean)
  )
    throw Error("SESPARSE 活动头部无效。");
  if (state.getBigUint64(24, true) !== 0n)
    throw Error("SESPARSE 有待回放日志，请先在 VMware 中处理并关闭镜像。");
  const payloadStart = BigInt(payload.at);
  const payloadLimit = BigInt(Math.min(payload.end, fileSize));
  return {
    grain,
    directory,
    tableLength,
    metadata,
    tableLocation(dir, index) {
      const entry = dir.getBigUint64(index * 8, true);
      if (entry === 0n) return 0;
      if (entry >> 32n !== 0x10000000n)
        throw Error("SESPARSE 粒度目录条目标志无效。");
      const at = tables.at + Number(entry & 0xffffffffn) * tableLength;
      vmdkRange(at, tableLength, tables.end);
      return at;
    },
    dataLocation(table, index) {
      const entry = table.getBigUint64(index * 8, true);
      if (entry === 0n) return INHERIT;
      const state = entry >> 60n;
      if (state === 1n || state === 2n) return ZERO;
      if (state !== 3n) throw Error("SESPARSE 粒度分配状态无效。");
      const slot =
        ((entry & 0xffffffffffffn) << 12n) | ((entry >> 48n) & 0xfffn);
      const at = payloadStart + slot * BigInt(grain);
      if (at + BigInt(grain) > payloadLimit)
        throw Error("SESPARSE 粒度数据超出已存储区段。");
      return Number(at);
    },
  };
}

export async function openEsxiSparse(
  file: ImageFile,
  kind: Kind,
  size: number,
  budget: VmdkBudget,
  parent?: LogicalImage["read"],
): Promise<LogicalImage> {
  const reader = vmdkFileReads(file, budget);
  const layout =
    kind === "VMFSSPARSE"
      ? cowd(await reader.capture(0, 4 * SECTOR), size, file.size)
      : await sesparse(reader, size, file.size);
  const { grain, directory, tableLength, metadata } = layout;
  const grainCount = Math.ceil(size / grain);
  const directoryCount = Math.ceil(grainCount / TABLE_ENTRIES);
  const dirBytes = await reader.capture(
    directory.at,
    directory.end - directory.at,
  );
  if (
    kind === "SESPARSE" &&
    dirBytes.subarray(directoryCount * 8).some(Boolean)
  )
    throw Error("SESPARSE 逻辑容量之外有已分配粒度表。");
  const dir = view(dirBytes);
  const locations = new Map<number, number>();
  const claimed = new Set<number>();
  for (let i = 0; i < directoryCount; i++) {
    const at = layout.tableLocation(dir, i);
    if (!at) continue;
    if (claimed.has(at)) throw Error("ESXi VMDK 重复引用同一粒度表。");
    claimed.add(at);
    locations.set(i, at);
    chargeVmdkTables(budget, tableLength);
    if (kind === "VMFSSPARSE") metadata.push({ at, end: at + tableLength });
  }
  metadata.sort((a, b) => a.at - b.at);
  for (let i = 1; i < metadata.length; i++)
    if (metadata[i].at < metadata[i - 1].end)
      throw Error("ESXi VMDK 元数据区段重叠。");

  const tables = new Map<number, DataView>();
  const addresses = new Float64Array(locations.size * TABLE_ENTRIES);
  let allocated = 0;
  for (const [index, at] of locations) {
    const table = view(await reader.capture(at, tableLength));
    tables.set(index, table);
    for (let j = 0; j < TABLE_ENTRIES; j++) {
      const location = layout.dataLocation(table, j);
      if (index * TABLE_ENTRIES + j >= grainCount && location !== INHERIT)
        throw Error("ESXi VMDK 逻辑容量之外有粒度条目。");
      if (location <= 0) continue;
      let lo = 0,
        hi = metadata.length;
      while (lo < hi) {
        const mid = (lo + hi) >>> 1;
        if (metadata[mid].at < location + grain) lo = mid + 1;
        else hi = mid;
      }
      if (lo && metadata[lo - 1].end > location)
        throw Error("ESXi VMDK 数据粒度与元数据重叠。");
      addresses[allocated++] = location;
    }
  }
  const sorted = addresses.subarray(0, allocated).sort();
  for (let i = 1; i < sorted.length; i++)
    if (sorted[i] - sorted[i - 1] < grain)
      throw Error("ESXi VMDK 数据粒度重复或重叠。");

  function mapped(index: number) {
    const table = tables.get(Math.floor(index / TABLE_ENTRIES));
    return table ? layout.dataLocation(table, index % TABLE_ENTRIES) : INHERIT;
  }
  let logicalBytes = 0;
  return {
    size,
    sectorSize: SECTOR,
    format: `VMDK · ${kind}`,
    warnings: [],
    notes: ["已检查 ESXi VMDK 的容器范围和粒度映射。数据粒度没有独立校验和。"],
    async read(at, length) {
      vmdkRange(at, length, size);
      logicalBytes += length;
      if (length > MiB || logicalBytes > logicalLimit(budget.stream, size))
        throw Error("仅允许读取 VMDK 分区表所需的有限字节。");
      const bytes = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const logical = at + done;
        const within = logical % grain;
        const index = Math.floor(logical / grain);
        const location = mapped(index);
        let run = Math.min(length - done, grain - within);
        for (let next = index + 1; done + run < length; next++) {
          const adjacent =
            location <= 0 ? location : location + (next - index) * grain;
          if (mapped(next) !== adjacent) break;
          run = Math.min(length - done, run + grain);
        }
        if (location > 0)
          bytes.set(await reader.physical(location + within, run), done);
        else if (location === INHERIT && parent)
          bytes.set(await parent(logical, run), done);
        done += run;
      }
      return bytes;
    },
    verify: reader.verify,
  };
}

// MS-VHDX v1 standalone images. No devices, filesystem paths, mounts, log writes
// or parent-file lookup. All I/O is bounded slices of the caller's selected File.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
const MiB = 1024 ** 2,
  KiB = 1024;
const BAT_LIMIT = 16 * MiB;
const REG_BAT = "6677c22d23f600429d64115e9bfd4a08";
const REG_META = "06a27c8b90479a4bb8fe575f050f886e";
const PARAMS = "3767a1ca36fa434db3b633f0aa44e76b";
const SIZE = "2442a52f1bcd7648b2115dbed83bf4b8";
const ID = "ab12cabee6b2234593efc309e000c746";
const LOGICAL = "1dbf41816fa90947ba47f233a8faab5f";
const PHYSICAL = "c748a3cd5d4471449cc9e9885251c556";
const PARENT = "2d5fd3a80bb34d45abf7d3d84834ab0c";
const KNOWN = new Map([
  [PARAMS, [8, 4]],
  [SIZE, [8, 6]],
  [ID, [16, 6]],
  [LOGICAL, [4, 6]],
  [PHYSICAL, [4, 6]],
]);
const dv = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u16 = (b: Uint8Array, at: number) => dv(b).getUint16(at, true);
const u32 = (b: Uint8Array, at: number) => dv(b).getUint32(at, true);
const u64 = (b: Uint8Array, at: number) => {
  const n = dv(b).getBigUint64(at, true);
  if (n > BigInt(Number.MAX_SAFE_INTEGER))
    throw new Error("VHDX 偏移或容量超出安全范围。");
  return Number(n);
};
const guid = (b: Uint8Array, at: number) =>
  Array.from(b.subarray(at, at + 16), (n) =>
    n.toString(16).padStart(2, "0"),
  ).join("");
const magic = (b: Uint8Array, s: string) =>
  [...s].every((c, i) => b[i] === c.charCodeAt(0));
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((n, i) => n === b[i]);
const crcTable = Uint32Array.from({ length: 256 }, (_, n) => {
  for (let bit = 0; bit < 8; bit++) n = (n >>> 1) ^ (n & 1 ? 0x82f63b78 : 0);
  return n >>> 0;
});
export function crc32c(bytes: Uint8Array, zeroAt = -1) {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) {
    const value = zeroAt >= 0 && i >= zeroAt && i < zeroAt + 4 ? 0 : bytes[i];
    crc = (crc >>> 8) ^ crcTable[(crc ^ value) & 255];
  }
  return ~crc >>> 0;
}
const valid = (bytes: Uint8Array, signature: string) =>
  magic(bytes, signature) && crc32c(bytes, 4) === u32(bytes, 4);
type Span = { at: number; end: number };
function bounds(at: number, length: number, size: number) {
  if (
    !Number.isSafeInteger(at) ||
    !Number.isSafeInteger(length) ||
    at < 0 ||
    length < 1 ||
    at > size - length
  )
    throw new Error("VHDX 读取范围越界。");
}
function disjoint(spans: Span[], label: string) {
  spans.sort((a, b) => a.at - b.at);
  for (let i = 1; i < spans.length; i++)
    if (spans[i].at < spans[i - 1].end) throw new Error(`VHDX ${label}重叠。`);
}
function intersects(spans: Span[], at: number, end: number) {
  let lo = 0,
    hi = spans.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (spans[mid].at < end) lo = mid + 1;
    else hi = mid;
  }
  return lo > 0 && spans[lo - 1].end > at;
}

export async function openVhdx(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < MiB ||
    file.size > 2 ** 43 + 512 * MiB ||
    file.size % MiB !== 0
  )
    throw new Error("VHDX 文件长度无效或超过当前支持范围。");
  let bytesRead = 0,
    calls = 0,
    logicalRead = 0;
  const physical = async (at: number, length: number) => {
    bounds(at, length, file.size);
    bytesRead += length;
    if (
      length > BAT_LIMIT ||
      bytesRead >
        (stream ? file.size * fileFactor(stream) + 40 * MiB : 40 * MiB) ||
      ++calls > (stream ? (file.size / 512) * fileFactor(stream) + 4096 : 4096)
    )
      throw new Error("VHDX 分区表读取超过资源上限。");
    const bytes = new Uint8Array(
      await file.slice(at, at + length).arrayBuffer(),
    );
    if (bytes.length !== length)
      throw new Error("VHDX 文件读取不完整，请重新打开离线副本。");
    return bytes;
  };
  const metadata: { at: number; bytes: Uint8Array }[] = [];
  const meta = async (at: number, length: number) => {
    const bytes = await physical(at, length);
    metadata.push({ at, bytes });
    return bytes;
  };
  const warnings: string[] = [],
    notes: string[] = [];
  const prefix = await meta(0, 8);
  if (!magic(prefix, "vhdxfile")) throw new Error("未识别到 VHDX 文件标识。");
  const headers = await Promise.all([
    meta(64 * KiB, 4096),
    meta(128 * KiB, 4096),
  ]);
  const active = headers.filter((h) => valid(h, "head"));
  if (active.length === 0)
    throw new Error("VHDX 两份头部均未通过 CRC-32C 校验。");
  if (active.length === 2) {
    const a = dv(active[0]).getBigUint64(8, true),
      b = dv(active[1]).getBigUint64(8, true);
    if (a === b)
      throw new Error("VHDX 两份有效头部序号相同，无法确定当前版本。");
    if (a < b) active.reverse();
  } else
    warnings.push(
      "只有一份 VHDX 头部通过校验；已按有效副本只读解析，未修复源文件。",
    );
  const header = active[0];
  if (u16(header, 66) !== 1) throw new Error("VHDX 当前头部版本暂不支持。");
  if (header.subarray(48, 64).some(Boolean))
    throw new Error(
      "VHDX 有待处理日志；请使用正常关闭后的离线副本，当前不回放或修改日志。",
    );
  const reserved: Span[] = [{ at: 0, end: MiB }];
  const logAt = u64(header, 72),
    logLength = u32(header, 68);
  if (logAt % MiB !== 0 || logLength % MiB !== 0)
    throw new Error("VHDX 日志区域未按 MiB 对齐。");
  if (logLength) {
    bounds(logAt, logLength, file.size);
    if (logAt < MiB) throw new Error("VHDX 日志与头部重叠。");
    reserved.push({ at: logAt, end: logAt + logLength });
  }
  const tables = await Promise.all([
    meta(192 * KiB, 64 * KiB),
    meta(256 * KiB, 64 * KiB),
  ]);
  const regionTables = tables.filter((t) => valid(t, "regi"));
  if (!regionTables.length)
    throw new Error("VHDX 两份区域表均未通过 CRC-32C 校验。");
  if (regionTables.length === 1)
    warnings.push(
      "只有一份 VHDX 区域表通过校验；已使用有效副本，未写回源文件。",
    );
  type Region = Span & { id: string; length: number; required: number };
  const parseRegions = (t: Uint8Array) => {
    const count = u32(t, 8),
      ids = new Set<string>(),
      entries: Region[] = [];
    if (count < 2 || count > 2047) throw new Error("VHDX 区域表条目数量无效。");
    for (let i = 0; i < count; i++) {
      const pos = 16 + i * 32,
        id = guid(t, pos),
        at = u64(t, pos + 16),
        length = u32(t, pos + 24),
        required = u32(t, pos + 28);
      if (
        ids.has(id) ||
        at < MiB ||
        at % MiB !== 0 ||
        length % MiB !== 0 ||
        required > 1
      )
        throw new Error("VHDX 区域条目标识重复或字段无效。");
      bounds(at, length, file.size);
      ids.add(id);
      if (![REG_BAT, REG_META].includes(id) && required === 1)
        throw new Error("VHDX 包含不支持的必需区域。");
      entries.push({ id, at, length, end: at + length, required });
    }
    return entries.sort((a, b) => a.id.localeCompare(b.id));
  };
  const regions = parseRegions(regionTables[0]);
  if (
    regionTables.length === 2 &&
    JSON.stringify(regions) !== JSON.stringify(parseRegions(regionTables[1]))
  )
    throw new Error("VHDX 两份有效区域表内容冲突，未自动选择副本。");
  const batRegion = regions.find((r) => r.id === REG_BAT),
    metaRegion = regions.find((r) => r.id === REG_META);
  // QEMU also emits these recognized regions with Required=0. They are still
  // mandatory to our reader; this does not relax unknown-required rejection.
  if (!batRegion || !metaRegion)
    throw new Error("VHDX 缺少必需的 BAT 或元数据区域。");
  reserved.push(...regions);
  disjoint(reserved, "容器区域");
  const table = await meta(metaRegion.at, 64 * KiB);
  const count = u16(table, 10);
  if (
    !magic(table, "metadata") ||
    u16(table, 8) !== 0 ||
    table.subarray(12, 32).some(Boolean) ||
    count > 2047
  )
    throw new Error("VHDX 元数据表头无效。");
  const items = new Map<string, { at: number; length: number }>(),
    keys = new Set<string>(),
    spans: Span[] = [];
  let userCount = 0;
  for (let i = 0; i < count; i++) {
    const pos = 32 + i * 32,
      id = guid(table, pos),
      at = u32(table, pos + 16),
      length = u32(table, pos + 20),
      flags = u32(table, pos + 24);
    const user = flags & 1,
      key = `${id}:${user}`;
    if (
      flags > 7 ||
      u32(table, pos + 28) !== 0 ||
      keys.has(key) ||
      length > MiB ||
      (length === 0
        ? at !== 0
        : at < 64 * KiB || at > metaRegion.length - length)
    )
      throw new Error("VHDX 元数据条目重复、越界或字段无效。");
    keys.add(key);
    if (user && ++userCount > 1024)
      throw new Error("VHDX 用户元数据条目超过上限。");
    if (length) spans.push({ at, end: at + length });
    if (!user && id === PARENT)
      throw new Error("暂不支持差分 VHDX 的父盘链，未读取父盘路径。");
    const expected = !user ? KNOWN.get(id) : undefined;
    if (expected) {
      if (length !== expected[0] || flags !== expected[1])
        throw new Error("VHDX 必需元数据的长度或属性不匹配。");
      items.set(id, { at: metaRegion.at + at, length });
    } else if (flags & 4) throw new Error("VHDX 包含不支持的必需元数据。");
  }
  disjoint(spans, "元数据项");
  if (items.size !== 5) throw new Error("VHDX 必需元数据不完整。");
  const data = new Map<string, Uint8Array>();
  for (const [id, item] of items)
    data.set(id, await meta(item.at, item.length));
  const params = data.get(PARAMS)!,
    blockSize = u32(params, 0),
    flags = u32(params, 4);
  if (flags & 2) throw new Error("暂不支持差分 VHDX 的父盘链。");
  if (
    flags > 1 ||
    blockSize < MiB ||
    blockSize > 256 * MiB ||
    (blockSize & (blockSize - 1)) !== 0
  )
    throw new Error("VHDX 块大小或文件参数不受支持。");
  const size = u64(data.get(SIZE)!, 0),
    sector = u32(data.get(LOGICAL)!, 0),
    physicalSector = u32(data.get(PHYSICAL)!, 0);
  if (![512, 4096].includes(sector) || ![512, 4096].includes(physicalSector))
    throw new Error("VHDX 扇区大小暂不支持。");
  if (size < 4 * MiB || size > 2 ** 43 || size % sector !== 0)
    throw new Error("VHDX 逻辑容量须为 4 MiB 至 8 TiB 且按扇区对齐。");
  const blocks = Math.ceil(size / blockSize),
    ratio = (2 ** 23 * sector) / blockSize;
  const entries = blocks + Math.floor((blocks - 1) / ratio),
    length = entries * 8;
  if (length > batRegion.length || length > BAT_LIMIT)
    throw new Error("VHDX BAT 不完整或超过 16 MiB 读取上限。");
  if (
    flags === 1 &&
    file.size <
      blocks * blockSize + reserved.reduce((sum, r) => sum + r.end - r.at, 0)
  )
    throw new Error("固定 VHDX 文件长度不足以容纳全部数据块。");
  const bat = await meta(batRegion.at, length),
    addresses = new Float64Array(entries);
  let addressCount = 0;
  for (let i = 0; i < entries; i++) {
    const low = u32(bat, i * 8),
      high = u32(bat, i * 8 + 4),
      state = low & 7;
    const at = high * 2 ** 32 + (low >>> 20) * MiB,
      bitmap = (i + 1) % (ratio + 1) === 0;
    if (
      (low & 0xffff8) !== 0 ||
      ![0, 1, 2, 3, 6].includes(state) ||
      (!at && state === 6) ||
      (bitmap && (state !== 0 || at !== 0))
    )
      throw new Error(
        "VHDX BAT 状态、保留位或扇区位图无效；不支持部分存在的数据块。",
      );
    if ((state === 0 || state === 2) && at !== 0)
      throw new Error("VHDX 空白块含不支持的非零位置。");
    if (at) {
      bounds(at, blockSize, file.size);
      if (intersects(reserved, at, at + blockSize))
        throw new Error("VHDX 数据块与容器元数据重叠。");
      addresses[addressCount++] = at;
    }
  }
  const ordered = addresses.subarray(0, addressCount).sort();
  for (let i = 1; i < ordered.length; i++)
    if (ordered[i] < ordered[i - 1] + blockSize)
      throw new Error("VHDX 数据块重复或重叠。");
  let zeroNote = false;
  return {
    size,
    sectorSize: sector as 512 | 4096,
    format: flags === 1 ? "VHDX · 固定容量" : "VHDX · 动态扩展",
    warnings,
    notes,
    async read(at, length) {
      bounds(at, length, size);
      logicalRead += length;
      if (length > MiB || logicalRead > logicalLimit(stream, size))
        throw new Error("仅允许读取 VHDX 分区表所需的有限字节。");
      const output = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const logical = at + done,
          block = Math.floor(logical / blockSize),
          within = logical % blockSize;
        const pos = (block + Math.floor(block / ratio)) * 8,
          low = u32(bat, pos),
          state = low & 7;
        const run = Math.min(length - done, blockSize - within);
        if (state === 6) {
          const fileAt = u32(bat, pos + 4) * 2 ** 32 + (low >>> 20) * MiB;
          output.set(await physical(fileAt + within, run), done);
        } else if (state !== 2 && !zeroNote) {
          notes.push(
            "未分配、未定义或已解除映射的 VHDX 块按零读取；此视图不用于恢复这些块的旧内容。",
          );
          zeroNote = true;
        }
        done += run;
      }
      return output;
    },
    async verify() {
      for (const saved of metadata)
        if (!same(saved.bytes, await physical(saved.at, saved.bytes.length)))
          throw new Error(
            "读取期间 VHDX 元数据发生变化，请关闭使用它的虚拟机后重新打开离线副本。",
          );
    },
  };
}

// MS-VHDX v1. All I/O is bounded slices of explicitly selected files.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { MissingImageFilesError } from "./imageDependencies";
import { crc32c, replayVhdxLog, VHDX_LOG_LIMIT } from "./vhdxLog";
export { crc32c } from "./vhdxLog";
const MiB = 1024 ** 2,
  KiB = 1024;
const BAT_LIMIT = 16 * MiB;
const FILE_LIMIT = 2 ** 43 + 512 * MiB;
const REG_BAT = "6677c22d23f600429d64115e9bfd4a08";
const REG_META = "06a27c8b90479a4bb8fe575f050f886e";
const PARAMS = "3767a1ca36fa434db3b633f0aa44e76b";
const SIZE = "2442a52f1bcd7648b2115dbed83bf4b8";
const ID = "ab12cabee6b2234593efc309e000c746";
const LOGICAL = "1dbf41816fa90947ba47f233a8faab5f";
const PHYSICAL = "c748a3cd5d4471449cc9e9885251c556";
const PARENT = "2d5fd3a80bb34d45abf7d3d84834ab0c";
const VHDX_LOCATOR = "b7ef4ab09ed1814ab78925b8e9445913";
const NULL_LINKAGE = "{00000000-0000-0000-0000-000000000000}";
const MAX_LAYERS = 16;
const CHAIN_METADATA_LIMIT = 64 * MiB;
const BITMAP_BITS = 8 * MiB;
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

function documentGuid(bytes: Uint8Array, at: number) {
  const a = u32(bytes, at).toString(16).padStart(8, "0"),
    b = u16(bytes, at + 4)
      .toString(16)
      .padStart(4, "0"),
    c = u16(bytes, at + 6)
      .toString(16)
      .padStart(4, "0"),
    tail = guid(bytes.subarray(at + 8, at + 16), 0);
  return [a, b, c, tail.slice(0, 4), tail.slice(4)].join("-");
}

// MS-VHDX 2.6.2.6. Use only basenames as hints for already selected files.
function parentLinkage(bytes: Uint8Array) {
  const count = u16(bytes, 18),
    end = 20 + count * 12;
  if (
    guid(bytes, 0) !== VHDX_LOCATOR ||
    u16(bytes, 16) !== 0 ||
    end > bytes.length
  )
    throw Error("VHDX 父盘定位元数据无效或类型不受支持。");
  const entries = new Map<string, string>();
  const decoder = new TextDecoder("utf-16le", { fatal: true, ignoreBOM: true });
  let textBytes = 0;
  const field = (at: number, length: number) => {
    if (
      at < end ||
      length === 0 ||
      length % 2 ||
      at > bytes.length - length ||
      (textBytes += length) > bytes.length
    )
      throw Error("VHDX 父盘定位字段越界或长度无效。");
    const value = decoder.decode(bytes.subarray(at, at + length));
    if (value.includes("\0")) throw Error("VHDX 父盘定位字段包含空字符。");
    return value;
  };
  for (let i = 0; i < count; i++) {
    const entry = 20 + i * 12;
    const key = field(u32(bytes, entry), u16(bytes, entry + 8));
    const value = field(u32(bytes, entry + 4), u16(bytes, entry + 10));
    if (entries.has(key)) throw Error("VHDX 父盘定位键重复。");
    entries.set(key, value);
  }
  const linkage = entries.get("parent_linkage")?.toLowerCase();
  const secondary = entries.get("parent_linkage2")?.toLowerCase();
  const paths = ["relative_path", "volume_path", "absolute_win32_path"]
    .map((key) => entries.get(key))
    .filter((path) => path !== undefined);
  if (
    !linkage ||
    !/^\{[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}\}$/.test(linkage) ||
    !/[1-9a-f]/.test(linkage) ||
    // Windows-created children persist an unused merge link as a zero GUID.
    (secondary !== undefined &&
      secondary !== NULL_LINKAGE &&
      secondary !== linkage) ||
    paths.length === 0
  )
    throw Error("VHDX 父盘标识无效或处于未完成合并状态。");
  const names = paths
    .map((path) => path.split(/[\\/]/).at(-1) ?? "")
    .filter(
      (name) =>
        /\.a?vhdx$/i.test(name) &&
        name.length <= 255 &&
        !/[:\x00-\x1f]/.test(name),
    );
  return {
    id: linkage.slice(1, -1),
    names: [...new Set(names.map((name) => name.toLowerCase()))],
  };
}

interface Layer extends LogicalImage {
  dataId: string;
  parent?: ReturnType<typeof parentLinkage>;
  physicalSector: number;
  read(
    at: number,
    length: number,
    parent?: LogicalImage["read"],
  ): Promise<Uint8Array>;
}

export async function openVhdx(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  const layer = await openLayer(file, stream, { metadata: 0 });
  if (layer.parent) throw Error("差分 VHDX 需要完整父盘链。");
  return layer;
}

async function openLayer(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode,
  budget: { metadata: number },
): Promise<Layer> {
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < MiB ||
    file.size > FILE_LIMIT
  )
    throw new Error("VHDX 文件长度无效或超过当前支持范围。");
  let bytesRead = 0,
    calls = 0,
    logicalRead = 0,
    logReadBudget = 0;
  let replay: ReturnType<typeof replayVhdxLog> | undefined;
  const physical = async (at: number, length: number) => {
    bounds(at, length, file.size);
    bytesRead += length;
    if (
      length > BAT_LIMIT ||
      bytesRead >
        40 * MiB + logReadBudget + (stream ? file.size * fileFactor(stream) : 0) ||
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
  const readRange = async (at: number, length: number) => {
    bounds(at, length, replay?.size ?? file.size);
    if (at + length <= file.size) return physical(at, length);
    const bytes = new Uint8Array(length);
    // Replay can extend the virtual file. Its unwritten suffix reads as zero.
    if (at < file.size) bytes.set(await physical(at, file.size - at));
    return bytes;
  };
  const metadata: { at: number; bytes: Uint8Array }[] = [];
  const meta = async (at: number, length: number) => {
    budget.metadata += length;
    if (budget.metadata > CHAIN_METADATA_LIMIT)
      throw Error("VHDX 链元数据超过 64 MiB 上限。");
    const bytes = await readRange(at, length);
    metadata.push({ at, bytes });
    return replay?.apply(bytes, at) ?? bytes;
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
  const logGuid = header.subarray(48, 64);
  const needsReplay = logGuid.some(Boolean);
  if (!needsReplay && file.size % MiB !== 0)
    throw Error("VHDX 文件长度无效或超过当前支持范围。");
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
  if (needsReplay) {
    if (u16(header, 64) !== 0 || logLength === 0 || logLength > VHDX_LOG_LIMIT)
      throw Error("VHDX 日志版本或大小不受支持（最多 16 MiB）。");
    logReadBudget = 2 * logLength;
    replay = replayVhdxLog(await meta(logAt, logLength), logGuid, logAt, file.size, FILE_LIMIT);
    warnings.push("已在内存中回放 VHDX 容器日志，源镜像保持只读。");
  }
  const viewSize = replay?.size ?? file.size;
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
      bounds(at, length, viewSize);
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
    const expected = !user ? KNOWN.get(id) : undefined;
    if (expected) {
      if (length !== expected[0] || flags !== expected[1])
        throw new Error("VHDX 必需元数据的长度或属性不匹配。");
      items.set(id, { at: metaRegion.at + at, length });
    } else if (!user && id === PARENT) {
      if (length < 20 || flags !== 4)
        throw Error("VHDX 父盘定位元数据长度或属性无效。");
      items.set(id, { at: metaRegion.at + at, length });
    } else if (flags & 4) throw new Error("VHDX 包含不支持的必需元数据。");
  }
  disjoint(spans, "元数据项");
  if ([...KNOWN.keys()].some((id) => !items.has(id)))
    throw new Error("VHDX 必需元数据不完整。");
  const data = new Map<string, Uint8Array>();
  for (const [id, item] of items)
    data.set(id, await meta(item.at, item.length));
  const params = data.get(PARAMS)!,
    blockSize = u32(params, 0),
    flags = u32(params, 4);
  const hasParent = (flags & 2) !== 0;
  if (
    flags > 3 ||
    blockSize < MiB ||
    blockSize > 256 * MiB ||
    (blockSize & (blockSize - 1)) !== 0
  )
    throw new Error("VHDX 块大小或文件参数不受支持。");
  if (hasParent !== data.has(PARENT))
    throw Error("VHDX 差分标记与父盘定位元数据不一致。");
  const parent = hasParent ? parentLinkage(data.get(PARENT)!) : undefined;
  const size = u64(data.get(SIZE)!, 0),
    sector = u32(data.get(LOGICAL)!, 0),
    physicalSector = u32(data.get(PHYSICAL)!, 0);
  if (![512, 4096].includes(sector) || ![512, 4096].includes(physicalSector))
    throw new Error("VHDX 扇区大小暂不支持。");
  if (size < 4 * MiB || size > 2 ** 43 || size % sector !== 0)
    throw new Error("VHDX 逻辑容量须为 4 MiB 至 8 TiB 且按扇区对齐。");
  const blocks = Math.ceil(size / blockSize),
    ratio = (BITMAP_BITS * sector) / blockSize;
  const entries = hasParent
      ? Math.ceil(blocks / ratio) * (ratio + 1)
      : blocks + Math.floor((blocks - 1) / ratio),
    length = entries * 8;
  if (length > batRegion.length || length > BAT_LIMIT)
    throw new Error("VHDX BAT 不完整或超过 16 MiB 读取上限。");
  if (
    flags === 1 &&
    viewSize <
      blocks * blockSize + reserved.reduce((sum, r) => sum + r.end - r.at, 0)
  )
    throw new Error("固定 VHDX 文件长度不足以容纳全部数据块。");
  const bat = await meta(batRegion.at, length),
    batView = dv(bat);
  const stateOf = (index: number) => batView.getUint32(index * 8, true) & 7;
  const atOf = (index: number) =>
    batView.getUint32(index * 8 + 4, true) * 2 ** 32 +
    (batView.getUint32(index * 8, true) >>> 20) * MiB;
  // Encode the extent size in the otherwise zero low bits of each aligned
  // offset. This bounds overlap checking even at the maximum BAT size.
  const allocations = new Float64Array(entries);
  let addressCount = 0;
  for (let i = 0; i < entries; i++) {
    const state = stateOf(i),
      at = atOf(i);
    const chunk = Math.floor(i / (ratio + 1));
    const payload = i - chunk * (ratio + 1),
      bitmap = payload === ratio;
    if ((batView.getUint32(i * 8, true) & 0xffff8) !== 0)
      throw Error("VHDX BAT 保留位无效。");
    if (bitmap) {
      if (state !== 0 && !(hasParent && state === 6))
        throw Error("VHDX 扇区位图 BAT 状态无效。");
    } else {
      if (
        (![0, 1, 2, 3, 6].includes(state) && !(hasParent && state === 7)) ||
        (chunk * ratio + payload >= blocks && (state !== 0 || at !== 0))
      )
        throw Error("VHDX BAT 数据块状态无效。");
      if (state === 7 && stateOf(chunk * (ratio + 1) + ratio) !== 6)
        throw Error("部分存在的 VHDX 数据块缺少扇区位图。");
    }
    if ((state === 6 || state === 7) && at === 0)
      throw Error("VHDX BAT 已分配块缺少磁盘偏移。");
    if ((state === 0 || state === 2) && at !== 0)
      throw new Error("VHDX 空白块含不支持的非零位置。");
    if (at) {
      const extent = bitmap ? MiB : blockSize;
      bounds(at, extent, viewSize);
      if (intersects(reserved, at, at + extent))
        throw new Error("VHDX 数据块或扇区位图与容器元数据重叠。");
      if (!bitmap && replay?.intersects(at, at + extent))
        throw Error("VHDX 日志试图覆盖有效载荷块。");
      allocations[addressCount++] = at + extent / MiB;
    }
  }
  let extentEnd = 0;
  for (const entry of allocations.subarray(0, addressCount).sort()) {
    const at = Math.floor(entry / MiB) * MiB;
    if (at < extentEnd) throw new Error("VHDX 数据块或扇区位图重复或重叠。");
    extentEnd = at + (entry - at) * MiB;
  }

  // Demand-load bitmaps. Retain only the current bitmap and the digests of
  // all used bitmaps, so the entire mapping can be checked before publication.
  let lastBitmap: { at: number; bytes: Uint8Array } | undefined;
  const bitmapDigests = new Map<number, Uint8Array>();
  const digest = async (bytes: Uint8Array<ArrayBuffer>) =>
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  async function getBitmap(at: number) {
    if (lastBitmap?.at === at) return lastBitmap.bytes;
    const raw = await readRange(at, MiB);
    const observed = await digest(raw),
      before = bitmapDigests.get(at);
    if (before && !same(before, observed))
      throw Error("读取期间 VHDX 扇区位图发生变化。");
    bitmapDigests.set(at, observed);
    const bytes = replay?.apply(raw, at) ?? raw;
    lastBitmap = { at, bytes };
    return bytes;
  }
  let zeroNote = false;
  return {
    size,
    sectorSize: sector as 512 | 4096,
    physicalSector,
    dataId: documentGuid(header, 32),
    parent,
    format: hasParent
      ? "VHDX · 差分链"
      : flags === 1
        ? "VHDX · 固定容量"
        : "VHDX · 动态扩展",
    warnings,
    notes,
    async read(at, length, parent) {
      bounds(at, length, size);
      if (hasParent && !parent) throw Error("差分 VHDX 需要完整父盘链。");
      logicalRead += length;
      if (length > MiB || logicalRead > logicalLimit(stream, size))
        throw new Error("仅允许读取 VHDX 分区表所需的有限字节。");
      const output = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const logical = at + done,
          block = Math.floor(logical / blockSize),
          within = logical % blockSize;
        const chunk = Math.floor(block / ratio),
          index = block + chunk;
        const state = stateOf(index);
        const run = Math.min(length - done, blockSize - within);
        if (state === 6) {
          output.set(await readRange(atOf(index) + within, run), done);
        } else if (state === 7) {
          const bitmapAt = atOf(chunk * (ratio + 1) + ratio);
          const chunkStart = chunk * BITMAP_BITS * sector;
          const bits = await getBitmap(bitmapAt);
          const present = (bit: number) =>
            (bits[bit >>> 3] & (1 << (bit & 7))) !== 0;
          const stop = logical + run;
          const endBit = Math.ceil((stop - chunkStart) / sector);
          for (let start = logical; start < stop;) {
            const firstBit = Math.floor((start - chunkStart) / sector);
            const stored = present(firstBit);
            let nextBit = firstBit + 1;
            while (nextBit < endBit && present(nextBit) === stored) nextBit++;
            const count = Math.min(stop, chunkStart + nextBit * sector) - start;
            output.set(
              stored
                ? await readRange(atOf(index) + within + start - logical, count)
                : await parent!(start, count),
              start - at,
            );
            start += count;
          }
        } else if (state === 0 && parent) {
          output.set(await parent(logical, run), done);
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
        if (!same(saved.bytes, await readRange(saved.at, saved.bytes.length)))
          throw new Error(
            "读取期间 VHDX 元数据发生变化，请关闭使用它的虚拟机后重新打开离线副本。",
          );
      for (const [at, expected] of bitmapDigests)
        if (!same(expected, await digest(await readRange(at, MiB))))
          throw Error("读取期间 VHDX 扇区位图发生变化。");
    },
  };
}

export async function openVhdxChain(
  file: ImageFile,
  parents: ImageFile[],
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (parents.length >= MAX_LAYERS)
    throw Error("VHDX 差分链最多包含 16 层（含当前镜像）。");
  const budget = { metadata: 0 },
    leaf = await openLayer(file, stream, budget);
  if (!leaf.parent && parents.length === 0) return leaf;
  const first = { file, image: leaf };
  const candidates = [first];
  const names = new Set([file.name.toLowerCase()]);
  for (const parent of parents) {
    if (
      !/\.a?vhdx$/i.test(parent.name) ||
      parent.name.length > 255 ||
      /[\\/:\0]/.test(parent.name)
    )
      throw Error("父盘必须是所选的普通 VHDX/AVHDX 文件。");
    const name = parent.name.toLowerCase();
    if (names.has(name))
      throw Error("所选 VHDX 文件名重复，无法唯一核对父盘。");
    names.add(name);
    const image = await openLayer(parent, stream, budget);
    if (!/[1-9a-f]/.test(image.dataId))
      throw Error("所选 VHDX 父盘数据标识为空。");
    candidates.push({ file: parent, image });
  }
  const layers = [first];
  for (let child = first; child.image.parent;) {
    const expected = child.image.parent;
    const byId = candidates.filter(
      (candidate) => candidate.image.dataId === expected.id,
    );
    // A new child may have exactly the same logical data ID as its parent.
    // In that case, select by both ID and a locator basename, never by order.
    const matches =
      byId.length === 1 && byId[0] !== child
        ? byId
        : byId.filter((candidate) =>
            expected.names.includes(candidate.file.name.toLowerCase()),
          );
    if (matches.length > 1)
      throw Error("多份已选 VHDX 文件匹配同一父盘，无法确定读取顺序。");
    const parent = matches[0];
    if (!parent) {
      if (layers.length === MAX_LAYERS)
        throw Error("VHDX 差分链超过 16 层上限。");
      throw new MissingImageFilesError(
        expected.names.length ? expected.names : [expected.id],
        "parent",
      );
    }
    if (layers.includes(parent)) throw Error("VHDX 父盘链存在循环。");
    if (
      parent.image.size !== leaf.size ||
      parent.image.sectorSize !== leaf.sectorSize ||
      parent.image.physicalSector !== leaf.physicalSector
    )
      throw Error("VHDX 父盘容量或扇区大小与差分盘不一致。");
    layers.push(parent);
    child = parent;
  }
  if (layers.length !== parents.length + 1)
    throw Error("所选文件包含不属于此 VHDX 差分链的父盘。");
  const chain = layers.map((layer) => layer.file.name);
  function readLayer(
    index: number,
    at: number,
    length: number,
  ): Promise<Uint8Array> {
    const parent =
      index + 1 < layers.length
        ? (offset: number, count: number) => readLayer(index + 1, offset, count)
        : undefined;
    return layers[index].image.read(at, length, parent);
  }
  return {
    size: leaf.size,
    sectorSize: leaf.sectorSize,
    format: "VHDX · 差分链",
    fileCount: layers.length,
    fileSize: layers.reduce((size, layer) => size + layer.file.size, 0),
    chain,
    warnings: layers.flatMap((layer) => layer.image.warnings),
    notes: [
      "读取顺序：" + chain.join(" → ") + "。",
      "已按父盘数据标识及扇区几何核对 VHDX 差分链。",
    ],
    read: (at, length) => readLayer(0, at, length),
    async verify() {
      for (const layer of layers) await layer.image.verify();
    },
  };
}

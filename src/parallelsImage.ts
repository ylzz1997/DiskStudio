// Parallels HDS allocation map, read only from explicitly selected files.
import { invoke } from "@tauri-apps/api/core";
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
const MiB = 1024 ** 2,
  MAP_LIMIT = 32 * MiB;
export type ParallelsExtent = {
  size: number;
  cluster: number;
  reserveMetadata: (bytes: number) => void;
  parent?: LogicalImage["read"];
};
const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u32 = (b: Uint8Array, at: number) => view(b).getUint32(at, true);
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((v, i) => v === b[i]);
const align = (n: number, unit: number) => Math.ceil(n / unit) * unit;
function bounds(at: number, length: number, size: number) {
  if (
    !Number.isSafeInteger(at) ||
    !Number.isSafeInteger(length) ||
    at < 0 ||
    length < 1 ||
    at > size - length
  )
    throw Error("Parallels HDD 读取范围越界。");
}
function hasAddress(sorted: Uint32Array, entry: number) {
  let end = sorted.length,
    start = 0;
  while (start < end) {
    const i = Math.floor((start + end) / 2);
    if (sorted[i] < entry) start = i + 1;
    else end = i;
  }
  return sorted[start] === entry;
}
export async function openParallels(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
  extent?: ParallelsExtent,
): Promise<LogicalImage> {
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < 512 ||
    file.size % 512 !== 0
  )
    throw Error("Parallels HDD 文件长度无效或未按扇区对齐。");
  let fileRead = 0,
    calls = 0,
    logicalRead = 0;
  const physical = async (at: number, length: number) => {
    bounds(at, length, file.size);
    fileRead += length;
    if (
      length > MAP_LIMIT ||
      fileRead >
        (stream ? file.size * fileFactor(stream) + 80 * MiB : 80 * MiB) ||
      ++calls >
        (stream ? (file.size / 512) * fileFactor(stream) + 20000 : 20000)
    )
      throw Error("Parallels HDD 分区表读取超过资源上限。");
    const bytes = new Uint8Array(
      await file.slice(at, at + length).arrayBuffer(),
    );
    if (bytes.length !== length) throw Error("Parallels HDD 文件读取不完整。");
    return bytes;
  };
  const head = await physical(0, 64),
    signature = new TextDecoder().decode(head.subarray(0, 16));
  const legacy = signature === "WithoutFreeSpace";
  if (!legacy && signature !== "WithouFreSpacExt")
    throw Error(
      "未识别到 Parallels HDD 结构化镜像头部。打开目录包时请选择 DiskDescriptor.xml。",
    );
  if (u32(head, 16) !== 2) throw Error("Parallels HDD 头部类型暂不支持。");
  if (![0, 0x312e3276].includes(u32(head, 44)))
    throw Error(
      "Parallels HDD 未正常关闭或关闭状态未知，请使用关闭后的离线副本。",
    );
  if (u32(head, 52) > 1)
    throw Error("Parallels HDD 正在转换或含未知功能标志。");
  const extensionAt = Number(view(head).getBigUint64(56, true) * 512n);
  if (legacy && u32(head, 40) !== 0)
    throw Error("旧版 Parallels HDD 的保留容量字段非零，无法确认格式。");
  const sectors = view(head).getBigUint64(36, true),
    size64 = sectors * 512n;
  if (size64 < BigInt(extent ? 512 : 4 * MiB) || size64 > 2n ** 43n)
    throw Error("Parallels HDD 逻辑容量须为 4 MiB 至 8 TiB。");
  const size = Number(size64),
    cluster = u32(head, 28) * 512,
    entries = u32(head, 32),
    mapLength = entries * 4;
  if (cluster < 512 || cluster > 256 * MiB)
    throw Error("Parallels HDD 数据块大小暂不支持。");
  if (extent && (size !== extent.size || cluster !== extent.cluster))
    throw Error("Parallels 数据分卷的容量或块大小与描述文件不符。");
  if (mapLength > MAP_LIMIT)
    throw Error("Parallels HDD 块映射表超过 32 MiB 上限。");
  const needed = Math.ceil(size / cluster);
  if (entries < needed)
    throw Error("Parallels HDD 块映射数量不足以覆盖逻辑容量。");
  const minimum = align(64 + mapLength, legacy ? 512 : cluster),
    declared = u32(head, 48) * 512;
  const dataStart = legacy && declared === 0 ? minimum : declared;
  if (
    dataStart < minimum ||
    dataStart > file.size ||
    (!legacy && dataStart % cluster !== 0)
  )
    throw Error("Parallels HDD 数据区与头部/块映射重叠、未对齐或不完整。");
  if ((file.size - dataStart) % cluster !== 0)
    throw Error("Parallels HDD 物理尾块不完整，请检查离线副本。");
  const checkExtensionOffset = (at: number) => {
    bounds(at, cluster, file.size);
    if (at < dataStart || (at - dataStart) % cluster !== 0)
      throw Error("Parallels HDS 格式扩展与元数据重叠或未对齐。");
  };
  if (extensionAt) {
    if (cluster > 16 * MiB) throw Error("Parallels HDS 格式扩展超过 16 MiB。");
    checkExtensionOffset(extensionAt);
  }
  extent?.reserveMetadata(mapLength + (extensionAt ? cluster : 0));
  const map = await physical(64, mapLength),
    addresses = new Uint32Array(needed);
  const unit = legacy ? 512 : cluster;
  let count = 0;
  for (let i = 0; i < entries; i++) {
    const entry = u32(map, i * 4);
    if (entry === 0) continue;
    if (i >= needed) throw Error("Parallels HDD 容量之外存在分配条目。");
    if (u32(head, 52) & 1)
      throw Error("Parallels HDD 空盘标志与已分配数据冲突。");
    const at = entry * unit;
    bounds(at, cluster, file.size);
    if (at < dataStart || (at - dataStart) % cluster !== 0)
      throw Error("Parallels HDD 数据块与元数据重叠或未对齐。");
    addresses[count++] = entry;
  }
  const used = addresses.subarray(0, count).sort();
  for (let i = 1; i < count; i++)
    if ((used[i] - used[i - 1]) * unit < cluster)
      throw Error("Parallels HDD 数据块重复或重叠。");
  let extension: Uint8Array | undefined;
  const reserved = new Set<number>();
  if (extensionAt) {
    const reserve = (at: number) => {
      checkExtensionOffset(at);
      if (reserved.has(at) || hasAddress(used, at / unit))
        throw Error("Parallels HDS 格式扩展与数据块或其他扩展区重叠。");
      reserved.add(at);
    };
    reserve(extensionAt);
    extension = new Uint8Array(cluster + 8);
    view(extension).setBigUint64(0, sectors, true);
    extension.set(await physical(extensionAt, cluster), 8);
    const bitmapSectors = await invoke<number[]>(
      "image_parallels_extension",
      extension,
    );
    for (const sector of bitmapSectors) reserve(sector * 512);
  }
  const warnings = extent
    ? []
    : [
        "仅解析所选 HDD 单文件；未读取目录包的描述文件，无法确认快照依赖。空块按零解释，结果不代表完整快照。",
      ];
  if (file.size - dataStart > (count + reserved.size) * cluster)
    warnings.push(
      "HDD 数据区存在未被索引引用的块；未将其作为逻辑数据，也未回收或修改这些块。",
    );
  return {
    size,
    sectorSize: 512,
    format: legacy ? "Parallels HDD · 传统结构" : "Parallels HDD · 扩展结构",
    warnings,
    notes: [
      ...(!extent
        ? [
            "HDD 头部和块映射表没有内容校验和；检查不能证明整盘数据完好。未提供快照合并、挂载或源文件写回。",
          ]
        : []),
      ...(extension
        ? ["已校验 Parallels HDS 变更位图扩展，位图仅用于变更跟踪。"]
        : []),
    ],
    async read(at, length) {
      bounds(at, length, size);
      logicalRead += length;
      if (length > MiB || logicalRead > logicalLimit(stream, size))
        throw Error("仅允许读取 Parallels HDD 分区表所需的有限字节。");
      const out = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const offset = at + done,
          within = offset % cluster,
          entry = u32(map, Math.floor(offset / cluster) * 4),
          run = Math.min(length - done, cluster - within);
        if (entry) out.set(await physical(entry * unit + within, run), done);
        else if (extent?.parent)
          out.set(await extent.parent(offset, run), done);
        done += run;
      }
      return out;
    },
    async verify() {
      for (const [at, bytes] of [
        [0, head],
        [64, map],
        ...(extension ? [[extensionAt, extension.subarray(8)] as const] : []),
      ] as const)
        if (!same(bytes, await physical(at, bytes.length)))
          throw Error(
            "读取期间 Parallels HDD 头部或块映射发生变化，请重新打开离线副本。",
          );
    },
  };
}

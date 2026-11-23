// VDI 1.1/1.1+ images decoded from bounded selected-file slices.
// Header and block definitions follow Oracle VDICore.h.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { MissingImageFilesError } from "./imageDependencies";
const MiB = 1024 ** 2;
const MAP_LIMIT = 32 * MiB;
const CHAIN_MAP_LIMIT = 64 * MiB;
const MAX_LAYERS = 16;
const ZERO = 0xfffffffe;
const FREE = 0xffffffff;
const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u32 = (b: Uint8Array, at: number) => view(b).getUint32(at, true);
function uuid(b: Uint8Array, at: number) {
  const stored = b.subarray(at, at + 16);
  const fields = view(stored);
  const tail = Array.from(stored.subarray(8), (n) =>
    n.toString(16).padStart(2, "0"),
  ).join("");
  return [
    fields.getUint32(0, true).toString(16).padStart(8, "0"),
    fields.getUint16(4, true).toString(16).padStart(4, "0"),
    fields.getUint16(6, true).toString(16).padStart(4, "0"),
    tail.slice(0, 4),
    tail.slice(4),
  ].join("-");
}
const power = (n: number) => n > 0 && (n & (n - 1)) === 0;
function bounds(at: number, length: number, size: number) {
  if (
    !Number.isSafeInteger(at) ||
    !Number.isSafeInteger(length) ||
    at < 0 ||
    length < 1 ||
    at > size - length
  )
    throw new Error("VDI 读取范围越界。");
}
async function inspectVdi(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
) {
  if (!Number.isSafeInteger(file.size) || file.size < 456)
    throw new Error("VDI 文件长度无效。");
  let fileRead = 0,
    calls = 0;
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
      throw new Error("VDI 分区表读取超过资源上限。");
    const bytes = new Uint8Array(
      await file.slice(at, at + length).arrayBuffer(),
    );
    if (bytes.length !== length)
      throw new Error("VDI 文件读取不完整，请重新打开离线副本。");
    return bytes;
  };
  const pre = await physical(0, 76);
  if (u32(pre, 64) !== 0xbeda107f) throw new Error("未识别到 VDI 文件标识。");
  if (u32(pre, 68) !== 0x10001)
    throw new Error("当前仅支持 VDI 1.1 与 1.1+；旧版及未知版本暂不支持。");
  const headerLength = u32(pre, 72);
  if (![384, 400].includes(headerLength))
    throw new Error("VDI 头部长度或扩展版本暂不支持。");
  const rest = await physical(76, headerLength - 4);
  const header = new Uint8Array(72 + headerLength);
  header.set(pre);
  header.set(rest, 76);
  const type = u32(header, 76),
    flags = u32(header, 80);
  if (![1, 2, 4].includes(type) || flags > 1)
    throw new Error("VDI 镜像类型或功能标志暂不支持。");
  const creation = uuid(header, 392),
    modification = uuid(header, 408);
  const hasId = (offset: number) =>
    header.subarray(offset, offset + 16).some(Boolean);
  if (type === 4 ? !hasId(424) || !hasId(440) : hasId(424) || hasId(440))
    throw new Error("VDI 镜像类型与父盘标识不一致，或缺少父盘修改标识。");
  const parent =
    type === 4
      ? { creation: uuid(header, 424), modification: uuid(header, 440) }
      : undefined;
  if (
    u32(header, 360) !== 512 ||
    (headerLength === 400 && u32(header, 468) !== 512)
  )
    throw new Error("VDI 当前仅支持 512 B 逻辑扇区。");
  if (!hasId(392) || !hasId(408)) throw new Error("VDI 创建或修改标识为空。");
  const size64 = view(header).getBigUint64(368, true);
  if (size64 < BigInt(4 * MiB) || size64 > 2n ** 43n || size64 % 512n !== 0n)
    throw new Error("VDI 逻辑容量须为 4 MiB 至 8 TiB 且按 512 B 对齐。");
  const size = Number(size64),
    blockSize = u32(header, 376),
    extra = u32(header, 380);
  if (
    !power(blockSize) ||
    blockSize < 512 ||
    blockSize > 256 * MiB ||
    (extra !== 0 && (!power(extra) || extra < 512 || extra > MiB))
  )
    throw new Error("VDI 数据块或附加块头大小暂不支持。");
  const blocks = u32(header, 384),
    allocated = u32(header, 388);
  const mapAt = u32(header, 340),
    dataAt = u32(header, 344),
    mapLength = blocks * 4;
  if (blocks !== Math.ceil(size / blockSize) || allocated > blocks)
    throw new Error("VDI 容量、总块数或已分配块数不一致。");
  if (mapLength > MAP_LIMIT) throw new Error("VDI 块映射表超过 32 MiB 上限。");
  if (
    mapAt < header.length ||
    mapAt % 512 !== 0 ||
    dataAt % 512 !== 0 ||
    dataAt < mapAt + mapLength
  )
    throw new Error("VDI 头部、块映射表或数据区域重叠或未对齐。");
  bounds(mapAt, mapLength, file.size);
  const stride = blockSize + extra,
    dataEnd = dataAt + allocated * stride;
  if (!Number.isSafeInteger(dataEnd) || dataEnd > file.size)
    throw new Error("VDI 已分配数据区不完整。");
  if (type === 2 && allocated !== blocks)
    throw new Error("固定 VDI 未完整分配；当前不支持这种布局。");
  return {
    physical,
    header,
    type,
    size,
    blockSize,
    extra,
    blocks,
    allocated,
    mapAt,
    dataAt,
    mapLength,
    stride,
    dataEnd,
    creation,
    modification,
    parent,
  };
}

type VdiMetadata = Awaited<ReturnType<typeof inspectVdi>>;
async function openLayer(
  meta: VdiMetadata,
  fileSize: number,
  stream: ImageReadMode,
  parent?: LogicalImage,
): Promise<LogicalImage> {
  const {
    physical,
    header,
    type,
    size,
    blockSize,
    extra,
    blocks,
    allocated,
    mapAt,
    dataAt,
    mapLength,
    stride,
    dataEnd,
  } = meta;
  const map = await physical(mapAt, mapLength);
  // An allocated slot must appear exactly once. A bitset keeps memory bounded
  // even with millions of small logical blocks; physical order may be arbitrary.
  const seen = new Uint8Array(Math.ceil(allocated / 8));
  let mapped = 0;
  for (let block = 0; block < blocks; block++) {
    const slot = u32(map, block * 4);
    if (slot >= ZERO) continue;
    if (slot >= allocated) throw new Error("VDI 块映射超出已分配范围。");
    const mask = 1 << (slot % 8),
      byte = Math.floor(slot / 8);
    if (seen[byte] & mask) throw new Error("VDI 多个逻辑块指向同一数据块。");
    seen[byte] |= mask;
    mapped++;
  }
  if (mapped !== allocated)
    throw new Error("VDI 已分配块数与实际映射数量不一致。");
  const warnings: string[] = [],
    notes = ["VDI 头部与块映射表不含校验和；结构检查不能证明全部内容完好。"];
  if (fileSize > dataEnd)
    warnings.push(
      "VDI 已分配数据区之后存在额外字节；未将其作为逻辑磁盘内容，也未修改源文件。",
    );
  let freeNote = false,
    logicalRead = 0;
  return {
    size,
    sectorSize: 512,
    format:
      type === 2
        ? "VDI · 固定容量"
        : type === 4
          ? "VDI · 差分链"
          : "VDI · 动态扩展",
    warnings,
    notes,
    async read(at, length) {
      bounds(at, length, size);
      logicalRead += length;
      if (length > MiB || logicalRead > logicalLimit(stream, size))
        throw new Error("仅允许读取 VDI 分区表所需的有限字节。");
      const output = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const logical = at + done,
          within = logical % blockSize,
          index = Math.floor(logical / blockSize),
          slot = u32(map, index * 4);
        let run = Math.min(length - done, blockSize - within);
        if (slot < ZERO)
          output.set(
            await physical(dataAt + slot * stride + extra + within, run),
            done,
          );
        else if (slot === FREE) {
          if (parent) {
            for (
              let next = index + 1;
              done + run < length && u32(map, next * 4) === FREE;
              next++
            )
              run = Math.min(length - done, run + blockSize);
            output.set(await parent.read(logical, run), done);
          } else if (!freeNote) {
            notes.push(
              "未分配 VDI 块按零读取；此视图不用于恢复这些块的旧内容。",
            );
            freeNote = true;
          }
        }
        done += run;
      }
      return output;
    },
    async verify() {
      for (const [at, saved] of [
        [0, header],
        [mapAt, map],
      ] as const) {
        const now = await physical(at, saved.length);
        if (!saved.every((value, i) => value === now[i]))
          throw new Error(
            "读取期间 VDI 头部或块映射发生变化，请关闭使用它的虚拟机后重新打开离线副本。",
          );
      }
    },
  };
}

export async function openVdi(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  const meta = await inspectVdi(file, stream);
  if (meta.parent)
    throw new MissingImageFilesError([meta.parent.creation], "parent");
  return openLayer(meta, file.size, stream);
}

export async function openVdiChain(
  file: ImageFile,
  parents: ImageFile[] = [],
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (!parents.length) return openVdi(file, stream);
  if (parents.length >= MAX_LAYERS)
    throw new Error("VDI 差分链最多包含 16 层（含当前镜像）。");
  const leaf = { file, meta: await inspectVdi(file, stream) };
  const selected = new Map([[leaf.meta.creation, leaf]]);
  for (const parent of parents) {
    if (
      !/\.vdi$/i.test(parent.name) ||
      parent.name.length > 255 ||
      /[\\/:\x00-\x1f]/.test(parent.name)
    )
      throw new Error("父盘必须是所选的普通 VDI 文件。");
    const meta = await inspectVdi(parent, stream);
    if (selected.has(meta.creation))
      throw new Error("所选 VDI 创建 UUID 重复，无法唯一匹配父盘。");
    selected.set(meta.creation, { file: parent, meta });
  }

  const chain = [leaf],
    used = new Set([leaf.meta.creation]);
  for (let child = leaf; child.meta.parent;) {
    const id = child.meta.parent;
    if (used.has(id.creation)) throw new Error("VDI 父盘链存在循环。");
    const parent = selected.get(id.creation);
    if (!parent) throw new MissingImageFilesError([id.creation], "parent");
    if (
      parent.meta.size !== leaf.meta.size ||
      parent.meta.modification !== id.modification
    )
      throw new Error("VDI 父盘修改 UUID 或逻辑容量与差分盘不一致。");
    chain.push(parent);
    used.add(id.creation);
    child = parent;
  }
  if (chain.length !== selected.size)
    throw new Error("所选文件包含不属于此 VDI 差分链的父盘。");
  if (
    chain.reduce((sum, layer) => sum + layer.meta.mapLength, 0) >
    CHAIN_MAP_LIMIT
  )
    throw new Error("VDI 差分链块映射表合计超过 64 MiB 上限。");
  const fileSize = chain.reduce((sum, layer) => sum + layer.file.size, 0);
  if (!Number.isSafeInteger(fileSize))
    throw new Error("VDI 文件总长度超出安全范围。");
  const images: LogicalImage[] = [];
  for (const layer of [...chain].reverse())
    images.push(
      await openLayer(layer.meta, layer.file.size, stream, images.at(-1)),
    );
  const image = images.at(-1)!;
  const names = chain.map((layer) => layer.file.name);
  return {
    ...image,
    chain: names,
    fileSize,
    fileCount: chain.length,
    warnings: [...new Set(images.flatMap((layer) => layer.warnings))],
    notes: [
      `读取顺序：${names.join(" → ")}。`,
      "已按创建 UUID、修改 UUID 和逻辑容量核对 VDI 差分链。",
      ...image.notes,
    ],
    async verify() {
      for (const layer of images) await layer.verify();
    },
  };
}

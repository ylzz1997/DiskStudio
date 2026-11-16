// VDI 1.1/1.1+ standalone images, decoded from bounded selected-File slices.
// Header field layout: Oracle VDICore.h. No parent lookup, devices or writes.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
const MiB = 1024 ** 2;
const MAP_LIMIT = 32 * MiB;
const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u32 = (b: Uint8Array, at: number) => view(b).getUint32(at, true);
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
export async function openVdi(
  file: Pick<ImageFile, "size" | "slice">,
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (!Number.isSafeInteger(file.size) || file.size < 456)
    throw new Error("VDI 文件长度无效。");
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
  if ([3, 4].includes(type) || header.subarray(424, 456).some(Boolean))
    throw new Error("暂不支持撤销或差分 VDI 的父盘链，未查找父盘。");
  if (![1, 2].includes(type) || flags > 1)
    throw new Error("VDI 镜像类型或功能标志暂不支持。");
  if (
    u32(header, 360) !== 512 ||
    (headerLength === 400 && u32(header, 468) !== 512)
  )
    throw new Error("VDI 当前仅支持 512 B 逻辑扇区。");
  if (
    !header.subarray(392, 408).some(Boolean) ||
    !header.subarray(408, 424).some(Boolean)
  )
    throw new Error("VDI 创建或修改标识为空。");
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
  const map = await physical(mapAt, mapLength);
  // An allocated slot must appear exactly once. A bitset keeps memory bounded
  // even with millions of small logical blocks; physical order may be arbitrary.
  const seen = new Uint8Array(Math.ceil(allocated / 8));
  let mapped = 0;
  for (let block = 0; block < blocks; block++) {
    const slot = u32(map, block * 4);
    if (slot >= 0xfffffffe) continue;
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
  if (file.size > dataEnd)
    warnings.push(
      "VDI 已分配数据区之后存在额外字节；未将其作为逻辑磁盘内容，也未修改源文件。",
    );
  let freeNote = false;
  return {
    size,
    sectorSize: 512,
    format: type === 2 ? "VDI · 固定容量" : "VDI · 动态扩展",
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
          within = logical % blockSize;
        const slot = u32(map, Math.floor(logical / blockSize) * 4);
        const run = Math.min(length - done, blockSize - within);
        if (slot < 0xfffffffe)
          output.set(
            await physical(dataAt + slot * stride + extra + within, run),
            done,
          );
        else if (slot === 0xffffffff && !freeNote) {
          notes.push("未分配 VDI 块按零读取；此视图不用于恢复这些块的旧内容。");
          freeNote = true;
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

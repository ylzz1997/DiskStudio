// Offline VHD v1 reader. Its only I/O capability is slicing the user-selected
// File; no paths, mounting, device APIs, parent lookup or writes are involved.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
const MiB = 1024 ** 2;
const MAX_VIRTUAL = 2040 * 1024 ** 3;
const MAX_BAT = 8 * MiB;
const EMPTY = 0xffffffff;
const NONE = 0xffffffffffffffffn;
export interface VhdImage extends LogicalImage {
  size: number;
  format: "VHD · 固定容量" | "VHD · 动态扩展";
  read(offset: number, length: number): Promise<Uint8Array>;
  verify(): Promise<void>;
}
export interface VhdLayer extends LogicalImage {
  id: string;
  parent?: { id: string; timestamp: number; name: string };
  read(
    offset: number,
    length: number,
    parentRead?: (at: number, size: number) => Promise<Uint8Array>,
  ): Promise<Uint8Array>;
}
export const vhdBudget = (stream: ImageReadMode = false) => ({
  stream,
  byteLimit: 128 * MiB,
  callLimit: 100000,
  bytes: 0,
  calls: 0,
  metadata: 0,
  maps: 0,
  bitmaps: 0,
});
type Budget = ReturnType<typeof vhdBudget>;
const idString = (b: Uint8Array) =>
  Array.from(b, (v) => v.toString(16).padStart(2, "0")).join("");
const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u32 = (b: Uint8Array, at: number) => view(b).getUint32(at);
const u64 = (b: Uint8Array, at: number) => {
  const value = view(b).getBigUint64(at);
  if (value > BigInt(Number.MAX_SAFE_INTEGER))
    throw new Error("VHD 偏移或容量超出安全范围。");
  return Number(value);
};
const magic = (b: Uint8Array, text: string) =>
  text.split("").every((c, i) => b[i] === c.charCodeAt(0));
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((v, i) => v === b[i]);
function checksum(b: Uint8Array, field: number) {
  let sum = 0;
  for (let i = 0; i < b.length; i++)
    if (i < field || i >= field + 4) sum += b[i];
  if (~sum >>> 0 !== u32(b, field)) throw new Error("VHD 元数据校验和不匹配。");
}
function range(offset: number, length: number, size: number) {
  if (
    !Number.isSafeInteger(offset) ||
    !Number.isSafeInteger(length) ||
    offset < 0 ||
    length < 1 ||
    offset > size - length
  )
    throw new Error("VHD 读取范围越界。");
}
function overlaps(a: [number, number], b: [number, number]) {
  return a[0] < b[1] && b[0] < a[1];
}

export async function openVhd(
  file: Pick<ImageFile, "size" | "slice">,
): Promise<VhdImage> {
  const image = await openVhdLayer(file);
  if (image.parent)
    throw Error("差分 VHD 需要完整父盘链，不能当作独立磁盘读取。");
  return image as VhdImage;
}
export async function openVhdLayer(
  file: Pick<ImageFile, "size" | "slice">,
  budget: Budget = vhdBudget(),
): Promise<VhdLayer> {
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < 2048 ||
    file.size > 2 ** 41 ||
    file.size % 512 !== 0
  )
    throw new Error("VHD 文件长度无效；仅支持扇区对齐的 VHD v1 文件。");
  let readBytes = 0,
    readCalls = 0,
    logicalBytes = 0;
  if (budget.stream) {
    budget.byteLimit += file.size * fileFactor(budget.stream);
    budget.callLimit += (file.size / 512) * fileFactor(budget.stream);
  }
  const physical = async (offset: number, length: number) => {
    range(offset, length, file.size);
    readBytes += length;
    budget.bytes += length;
    if (
      length > MAX_BAT ||
      readBytes >
        (budget.stream
          ? file.size * fileFactor(budget.stream) + 32 * MiB
          : 32 * MiB) ||
      ++readCalls >
        (budget.stream
          ? (file.size / 512) * fileFactor(budget.stream) + 20000
          : 20000) ||
      budget.bytes > budget.byteLimit ||
      ++budget.calls > budget.callLimit
    )
      throw new Error("VHD 分区表读取超过资源上限。");
    const bytes = new Uint8Array(
      await file.slice(offset, offset + length).arrayBuffer(),
    );
    if (bytes.length !== length)
      throw new Error("VHD 文件读取不完整，请重新打开离线副本。");
    return bytes;
  };
  const metadata: { offset: number; bytes: Uint8Array }[] = [];
  const meta = async (offset: number, length: number) => {
    budget.metadata += length;
    if (budget.metadata > 32 * MiB)
      throw Error("VHD 链元数据超过 32 MiB 上限。");
    const bytes = await physical(offset, length);
    metadata.push({ offset, bytes });
    return bytes;
  };
  const footerAt = file.size - 512;
  const footer = await meta(footerAt, 512);
  if (!magic(footer, "conectix"))
    throw new Error(
      "未找到 VHD 文件尾；VHDX 需要独立解析，不能作为 VHD 打开。",
    );
  checksum(footer, 64);
  if (
    u32(footer, 12) !== 0x10000 ||
    ![2, 3].includes(u32(footer, 8)) ||
    footer.slice(85).some(Boolean)
  )
    throw new Error("VHD 版本、特性或保留字段暂不支持。");
  if (footer[84] !== 0)
    throw new Error("VHD 处于保存状态，请使用已关闭虚拟机的离线副本。");
  const type = u32(footer, 60);
  if (![2, 3, 4].includes(type))
    throw new Error("仅支持固定容量、动态扩展和差分 VHD。");
  const id = idString(footer.subarray(68, 84));
  const size = u64(footer, 48);
  const original = u64(footer, 40);
  if (
    size < 4 * MiB ||
    size > MAX_VIRTUAL ||
    size % 512 !== 0 ||
    original < 512 ||
    original > MAX_VIRTUAL ||
    original % 512 !== 0
  )
    throw new Error("VHD 逻辑容量须为 4 MiB 至 2040 GiB，且按 512 B 对齐。");
  // Older Virtual PC/QEMU images can give CHS and current_size different meanings.
  // Reject ambiguous capacity rather than silently moving the backup GPT window.
  const creator = String.fromCharCode(...footer.slice(28, 32));
  const chs = view(footer).getUint16(56) * footer[58] * footer[59] * 512;
  if (
    ["vpc ", "qemu"].includes(creator) &&
    chs !== size &&
    chs !== 65535 * 16 * 255 * 512
  )
    throw new Error(
      "此旧版 VHD 的 CHS 与逻辑容量不一致，暂不支持自动选择容量。",
    );
  const logicalRange = (offset: number, length: number) => {
    range(offset, length, size);
    logicalBytes += length;
    if (length > MiB || logicalBytes > logicalLimit(budget.stream, size))
      throw new Error("仅允许读取 VHD 分区表所需的有限字节。");
  };
  const verify = async () => {
    for (const saved of metadata) {
      if (!same(saved.bytes, await physical(saved.offset, saved.bytes.length)))
        throw new Error(
          "读取期间 VHD 元数据发生变化，请关闭使用它的虚拟机后重新打开副本。",
        );
    }
  };
  if (type === 2) {
    if (view(footer).getBigUint64(16) !== NONE || size !== footerAt)
      throw new Error("固定 VHD 的数据长度与文件尾不一致。");
    return {
      id,
      size,
      sectorSize: 512,
      warnings: [],
      notes: [],
      format: "VHD · 固定容量",
      verify,
      read: async (offset, length) => {
        logicalRange(offset, length);
        return physical(offset, length);
      },
    };
  }
  const headerAt = u64(footer, 16);
  if (headerAt < 512 || headerAt % 512 !== 0 || headerAt > footerAt - 1024)
    throw new Error("VHD 动态头部越界或未对齐。");
  const prefix = await meta(0, 512);
  if (!same(prefix, footer))
    throw new Error("VHD 首尾副本不一致，未自动选择或修复副本。");
  const header = await meta(headerAt, 1024);
  if (!magic(header, "cxsparse")) throw new Error("VHD 动态头部标识无效。");
  checksum(header, 36);
  if (
    view(header).getBigUint64(8) !== NONE ||
    u32(header, 24) !== 0x10000 ||
    (type === 3
      ? header.slice(40).some(Boolean)
      : header.slice(60, 64).some(Boolean) || header.slice(768).some(Boolean))
  )
    throw new Error("VHD 动态头部含不支持的版本、父盘信息或保留字段。");
  const blockSize = u32(header, 32);
  const entries = u32(header, 28);
  const batAt = u64(header, 16);
  if (
    blockSize < 512 ||
    blockSize > 256 * MiB ||
    (blockSize & (blockSize - 1)) !== 0
  )
    throw new Error("VHD 数据块大小暂不支持。");
  const required = Math.ceil(size / blockSize);
  if (entries < required || entries > MAX_BAT / 4)
    throw new Error("VHD 块分配表不完整或超过 8 MiB 上限。");
  const batSize = Math.ceil((entries * 4) / 512) * 512;
  budget.maps += batSize;
  if (budget.maps > 16 * MiB) throw Error("VHD 链分配表超过 16 MiB 上限。");
  if (
    batAt < 512 ||
    batAt % 512 !== 0 ||
    batAt > footerAt - batSize ||
    overlaps([batAt, batAt + batSize], [headerAt, headerAt + 1024])
  )
    throw new Error("VHD 块分配表越界、未对齐或与头部重叠。");
  const bat = await meta(batAt, batSize);
  const bitmapSize = Math.ceil(blockSize / 512 / 8 / 512) * 512;
  const occupied: [number, number][] = [
    [0, 512],
    [headerAt, headerAt + 1024],
    [batAt, batAt + batSize],
    [footerAt, file.size],
  ];
  let parent: VhdLayer["parent"];
  if (type === 4) {
    const parentId = idString(header.subarray(40, 56));
    if (/^0+$/.test(parentId) || /^0+$/.test(id) || parentId === id)
      throw Error("VHD 差分盘的自身或父盘标识无效、重复。");
    let parentName: string;
    try {
      parentName = new TextDecoder("utf-16be", { fatal: true })
        .decode(header.subarray(64, 576))
        .replace(/\0+$/, "");
    } catch {
      throw Error("VHD 父盘名称的 UTF-16 编码无效。");
    }
    if (/[\x00-\x1f\x7f]/.test(parentName))
      throw Error("VHD 父盘名称含控制字符。");
    // Display only a basename hint. Never use a locator/name as an I/O path.
    parentName = parentName.split(/[\\/]/).at(-1) ?? "";
    parent = { id: parentId, timestamp: u32(header, 56), name: parentName };
    for (let i = 0; i < 8; i++) {
      const loc = header.subarray(576 + i * 24, 600 + i * 24);
      if (!loc.some(Boolean)) continue;
      const code = String.fromCharCode(...loc.subarray(0, 4)),
        space = u32(loc, 4),
        length = u32(loc, 8),
        at = u64(loc, 16);
      // Windows VPC/Hyper-V and DiscUtils reserve bytes, despite old spec wording.
      if (
        !["W2ru", "W2ku"].includes(code) ||
        u32(loc, 12) ||
        space < 512 ||
        space > 65536 ||
        space % 512 ||
        length < 2 ||
        length % 2 ||
        length > space ||
        at % 512 ||
        at > footerAt - space
      )
        throw Error("VHD 父盘定位记录的类型、长度或范围暂不支持。");
      if (occupied.some((span) => overlaps([at, at + space], span)))
        throw Error("VHD 父盘定位记录与元数据重叠。");
      occupied.push([at, at + space]);
      await meta(at, space); // Rechecked, but no contained path is opened.
    }
  }
  const blocks = new Uint32Array(entries);
  for (let i = 0; i < entries; i++) {
    const entry = u32(bat, i * 4);
    blocks[i] = entry;
    if (entry === EMPTY) continue;
    const at = entry * 512,
      end = at + bitmapSize + blockSize;
    if (i >= required || end > footerAt)
      throw new Error("VHD 已分配块越界或超出逻辑容量。");
    if (occupied.some((span) => overlaps([at, end], span)))
      throw new Error("VHD 数据块与元数据重叠。");
  }
  // Typed arrays avoid up to a million JS range objects for a large BAT.
  const allocated = blocks.filter((entry) => entry !== EMPTY).sort();
  for (let i = 1; i < allocated.length; i++)
    if (allocated[i] * 512 < allocated[i - 1] * 512 + bitmapSize + blockSize)
      throw new Error("VHD 数据块与其他块重叠。");
  const bitmaps = new Map<number, Uint8Array>();
  let bitmapBytes = 0;
  return {
    id,
    parent,
    size,
    sectorSize: 512,
    warnings: [],
    notes: [],
    format: parent ? "VHD · 差分盘" : "VHD · 动态扩展",
    verify,
    read: async (offset, length, parentRead) => {
      logicalRange(offset, length);
      if (parent && !parentRead)
        throw Error("VHD 父盘链不完整，不能用零填充缺失父盘。");
      const output = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const logical = offset + done,
          block = Math.floor(logical / blockSize),
          within = logical % blockSize;
        const count = Math.min(length - done, blockSize - within);
        if (blocks[block] === EMPTY) {
          if (parentRead) output.set(await parentRead(logical, count), done);
        } else {
          const at = blocks[block] * 512;
          let bitmap = bitmaps.get(block);
          if (!bitmap) {
            bitmapBytes += bitmapSize;
            budget.bitmaps += bitmapSize;
            if (
              !budget.stream &&
              (bitmapBytes > 4 * MiB || budget.bitmaps > 16 * MiB)
            )
              throw new Error("VHD 位图读取超过资源上限。");
            // Conversion verifies the entire logical source in a second fresh
            // pass; do not retain every data bitmap for multi-terabyte images.
            bitmap = await (budget.stream
              ? physical(at, bitmapSize)
              : meta(at, bitmapSize));
            bitmaps.set(block, bitmap);
            if (budget.stream)
              while (bitmaps.size * bitmapSize > 4 * MiB)
                bitmaps.delete(bitmaps.keys().next().value!);
          }
          // Coalesce equal bitmap states, including partial-sector boundaries.
          // Explicit child zero sectors override their parent's nonzero bytes.
          for (let copied = 0; copied < count;) {
            const pos = within + copied,
              sector = Math.floor(pos / 512);
            const present = Boolean(
              bitmap[Math.floor(sector / 8)] & (0x80 >>> (sector % 8)),
            );
            let run = Math.min(count - copied, 512 - (pos % 512));
            while (copied + run < count) {
              const next = Math.floor((pos + run) / 512);
              if (
                Boolean(
                  bitmap[Math.floor(next / 8)] & (0x80 >>> (next % 8)),
                ) !== present
              )
                break;
              run += Math.min(512, count - copied - run);
            }
            if (present)
              output.set(
                await physical(at + bitmapSize + pos, run),
                done + copied,
              );
            else if (parentRead)
              output.set(
                await parentRead(logical + copied, run),
                done + copied,
              );
            copied += run;
          }
        }
        done += count;
      }
      return output;
    },
  };
}

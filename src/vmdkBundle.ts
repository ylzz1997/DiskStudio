import {
  logicalLimit,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { parseVmdkDescriptor, vmdkNameKey } from "./vmdkDescriptor";
import { openVmdk, vmdkBudget } from "./vmdkImage";
import { MissingImageFilesError } from "./imageDependencies";
const MiB = 1024 ** 2;
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((v, i) => v === b[i]);
async function bytes(file: ImageFile, at: number, length: number) {
  const result = new Uint8Array(
    await file.slice(at, at + length).arrayBuffer(),
  );
  if (result.length !== length) throw Error("VMDK 文件读取不完整。");
  return result;
}
export class MissingVmdkFilesError extends MissingImageFilesError {
  constructor(missing: string[]) {
    super(missing);
  }
}
// Only File objects supplied by the user's pickers. No filesystem lookup or IPC paths.
export async function openVmdkSource(
  file: ImageFile,
  companions: ImageFile[] = [],
  inflate?: Parameters<typeof openVmdk>[1],
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (!Number.isSafeInteger(file.size) || file.size < 4)
    throw Error("VMDK 文件长度无效。");
  if (companions.length > 64) throw Error("最多选择 64 个 VMDK 数据文件。");
  const prefix = await bytes(file, 0, 4);
  if (same(prefix, new Uint8Array([75, 68, 77, 86])))
    return openVmdk(file, inflate, undefined, stream);
  if (file.size > 65536)
    throw Error("请选择 VMDK 主描述文件，描述符不能超过 64 KiB。");
  const descriptor = await bytes(file, 0, file.size),
    desc = parseVmdkDescriptor(descriptor);
  if (
    !["monolithicflat", "twogbmaxextentflat", "twogbmaxextentsparse"].includes(
      desc.type,
    )
  )
    throw Error("此 VMDK 类型需要嵌入描述符，不能作为外部描述文件打开。");
  const primaryKey = vmdkNameKey(file.name),
    selected = new Map<string, ImageFile>();
  if (desc.extents.some((e) => vmdkNameKey(e.name) === primaryKey))
    throw Error("VMDK 描述文件不能引用自身作为数据文件。");
  for (const part of companions) {
    const key = vmdkNameKey(part.name);
    if (selected.has(key) || key === primaryKey)
      throw Error("所选 VMDK 文件名重复，无法唯一匹配分卷。");
    selected.set(key, part);
  }
  const missing = [
    ...new Set(
      desc.extents
        .filter((e) => !selected.has(vmdkNameKey(e.name)))
        .map((e) => e.name),
    ),
  ];
  if (missing.length) throw new MissingVmdkFilesError(missing);
  // Validate every file/range before reading any companion. Repeated FLAT files
  // are allowed only for disjoint physical ranges; sparse aliases are rejected.
  const used = new Map<
    string,
    { file: ImageFile; ranges: { at: number; end: number; sparse: boolean }[] }
  >();
  for (const extent of desc.extents) {
    const key = vmdkNameKey(extent.name),
      part = selected.get(key)!;
    if (!Number.isSafeInteger(part.size) || part.size < 512 || part.size % 512)
      throw Error(`VMDK 数据文件长度无效：${extent.name}`);
    if (extent.kind === "FLAT" && extent.offset > part.size - extent.size)
      throw Error(`VMDK 平面数据范围越界：${extent.name}`);
    const record = used.get(key) ?? { file: part, ranges: [] };
    const range = {
      at: extent.offset,
      end: extent.offset + extent.size,
      sparse: extent.kind === "SPARSE",
    };
    if (
      record.ranges.some(
        (r) =>
          r.sparse || range.sparse || (range.at < r.end && range.end > r.at),
      )
    )
      throw Error("VMDK 多个范围重复引用或重叠，未合并读取。");
    record.ranges.push(range);
    used.set(key, record);
  }
  const budget = vmdkBudget(stream),
    parts: {
      start: number;
      size: number;
      file: ImageFile;
      offset: number;
      image?: LogicalImage;
    }[] = [];
  let start = 0,
    logicalBytes = 0;
  for (const extent of desc.extents) {
    const part = selected.get(vmdkNameKey(extent.name))!;
    const image =
      extent.kind === "SPARSE"
        ? await openVmdk(part, undefined, { size: extent.size, budget })
        : undefined;
    parts.push({
      start,
      size: extent.size,
      file: part,
      offset: extent.offset,
      image,
    });
    start += extent.size;
  }
  const fileSize =
    file.size + [...used.values()].reduce((n, r) => n + r.file.size, 0);
  if (!Number.isSafeInteger(fileSize))
    throw Error("VMDK 文件总长度超出安全范围。");
  const captured: { file: ImageFile; at: number; data: Uint8Array }[] = [];
  return {
    size: desc.size,
    sectorSize: 512,
    fileSize,
    fileCount: used.size + 1,
    format:
      desc.type === "monolithicflat"
        ? "VMDK · 平面数据"
        : desc.type === "twogbmaxextentflat"
          ? "VMDK · 分卷平面"
          : "VMDK · 分卷稀疏",
    warnings: [],
    notes: [
      `已按描述文件顺序组合 ${desc.extents.length} 个数据范围，仅使用明确选择的文件。`,
      "VMDK 没有完整内容校验和；同名文件匹配和结构检查不能证明数据文件属于同一副本，请选择同一离线镜像的完整文件组。",
      ...(selected.size > used.size
        ? ["未使用所选文件中的无关文件，未读取其内容。"]
        : []),
    ],
    async read(at, length) {
      if (
        !Number.isSafeInteger(at) ||
        !Number.isSafeInteger(length) ||
        at < 0 ||
        length < 1 ||
        at > desc.size - length
      )
        throw Error("VMDK 读取范围越界。");
      logicalBytes += length;
      if (length > MiB || logicalBytes > logicalLimit(stream, desc.size))
        throw Error("仅允许读取 VMDK 分区表所需的有限字节。");
      const result = new Uint8Array(length);
      for (const part of parts) {
        const begin = Math.max(at, part.start),
          end = Math.min(at + length, part.start + part.size);
        if (begin >= end) continue;
        const offset = begin - part.start;
        const data = part.image
          ? await part.image.read(offset, end - begin)
          : await bytes(part.file, part.offset + offset, end - begin);
        if (!part.image && !stream)
          captured.push({ file: part.file, at: part.offset + offset, data });
        result.set(data, begin - at);
      }
      return result;
    },
    async verify() {
      if (!same(descriptor, await bytes(file, 0, file.size)))
        throw Error("读取期间 VMDK 描述文件发生变化，请重新打开离线副本。");
      for (const part of parts) await part.image?.verify();
      for (const item of captured)
        if (!same(item.data, await bytes(item.file, item.at, item.data.length)))
          throw Error("读取期间 VMDK 平面数据发生变化，请重新打开离线副本。");
    },
  };
}

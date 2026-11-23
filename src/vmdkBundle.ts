import {
  logicalLimit,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { parseVmdkDescriptor, vmdkNameKey } from "./vmdkDescriptor";
import { openVmdk, vmdkBudget } from "./vmdkImage";
import { openEsxiSparse } from "./vmdkEsxi";
import { MissingImageFilesError } from "./imageDependencies";
const MiB = 1024 ** 2;
const MAX_CHAIN_LAYERS = 16;
type Descriptor = ReturnType<typeof parseVmdkDescriptor>;
type ReadParent = LogicalImage["read"];
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
  if (companions.length > 64) throw Error("最多选择 64 个 VMDK 数据文件。");
  const primaryKey = vmdkNameKey(file.name),
    selected = new Map([[primaryKey, file]]),
    used = new Set<string>(),
    layers: string[] = [],
    parentImages: LogicalImage[] = [],
    budget = vmdkBudget(stream);
  for (const companion of companions) {
    const key = vmdkNameKey(companion.name);
    if (selected.has(key))
      throw Error("所选 VMDK 文件名重复，无法唯一匹配父盘或分卷。");
    selected.set(key, companion);
  }
  const openLayer = async (
    layer: ImageFile,
    expected?: { cid: string; size: number },
  ): Promise<LogicalImage> => {
    const key = vmdkNameKey(layer.name);
    if (used.has(key)) throw Error("VMDK 快照链存在循环或重复引用镜像文件。");
    used.add(key);
    layers.push(layer.name);
    const bind = async (desc: Descriptor): Promise<ReadParent | undefined> => {
      if (
        expected &&
        (desc.cid !== expected.cid || desc.size !== expected.size)
      )
        throw Error("VMDK 父盘 CID 或逻辑容量与差分盘不一致。");
      if (!desc.parent) return;
      if (layers.length >= MAX_CHAIN_LAYERS)
        throw Error("VMDK 快照链最多包含 16 层（含当前镜像）。");
      const parent = selected.get(vmdkNameKey(desc.parent.name));
      if (!parent)
        throw new MissingImageFilesError([desc.parent.name], "parent");
      const image = await openLayer(parent, {
        cid: desc.parent.cid,
        size: desc.size,
      });
      parentImages.push(image);
      return image.read;
    };
    if (!Number.isSafeInteger(layer.size) || layer.size < 4)
      throw Error("VMDK 文件长度无效。");
    const signature = await bytes(layer, 0, 4);
    const magic = new DataView(
      signature.buffer,
      signature.byteOffset,
      4,
    ).getUint32(0, true);
    if (magic === 0x564d444b)
      return openVmdk(layer, inflate, undefined, stream, { budget, bind });
    if (magic === 0x44574f43 || magic === 0xcafebabe)
      throw Error("请选择 ESXi VMDK 主描述文件，并补选各层数据文件。");
    return openBundle(layer, selected, used, budget, bind);
  };
  const image = await openLayer(file);
  if (layers.length === 1 && image.fileCount === undefined) return image;
  const fileSize = [...used].reduce((n, key) => n + selected.get(key)!.size, 0);
  if (!Number.isSafeInteger(fileSize))
    throw Error("VMDK 文件总长度超出安全范围。");
  return {
    ...image,
    fileSize,
    fileCount: used.size,
    ...(layers.length > 1 ? { chain: layers, format: "VMDK · 快照链" } : {}),
    notes: [
      ...(layers.length > 1
        ? [
            `读取顺序：${layers.join(" → ")}。`,
            "已按原文件名、CID 和逻辑容量核对 VMDK 快照链。",
          ]
        : []),
      ...new Set([image, ...parentImages].flatMap((layer) => layer.notes)),
      ...(selected.size > used.size
        ? ["未使用所选文件中的无关文件，未读取其内容。"]
        : []),
    ],
    warnings: [
      ...new Set([image, ...parentImages].flatMap((layer) => layer.warnings)),
    ],
    async verify() {
      await image.verify();
      for (const parent of parentImages) await parent.verify();
    },
  };
}

async function openBundle(
  file: ImageFile,
  selected: Map<string, ImageFile>,
  consumed: Set<string>,
  budget: ReturnType<typeof vmdkBudget>,
  bind: (desc: Descriptor) => Promise<ReadParent | undefined>,
): Promise<LogicalImage> {
  if (file.size > 65536)
    throw Error("请选择 VMDK 主描述文件，描述符不能超过 64 KiB。");
  const descriptor = await bytes(file, 0, file.size),
    desc = parseVmdkDescriptor(descriptor);
  if (["monolithicsparse", "streamoptimized"].includes(desc.type))
    throw Error("此 VMDK 类型需要嵌入描述符，不能作为外部描述文件打开。");
  const primaryKey = vmdkNameKey(file.name);
  if (desc.extents.some((e) => vmdkNameKey(e.name) === primaryKey))
    throw Error("VMDK 描述文件不能引用自身作为数据文件。");
  const parent = await bind(desc);
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
    if (consumed.has(key))
      throw Error("VMDK 快照链重复使用描述文件或数据分卷。");
    if (!Number.isSafeInteger(part.size) || part.size < 512 || part.size % 512)
      throw Error(`VMDK 数据文件长度无效：${extent.name}`);
    const flat = extent.kind === "FLAT" || extent.kind === "VMFS";
    if (flat && extent.offset > part.size - extent.size)
      throw Error(`VMDK 平面数据范围越界：${extent.name}`);
    const record = used.get(key) ?? { file: part, ranges: [] };
    const range = {
      at: extent.offset,
      end: extent.offset + extent.size,
      sparse: !flat,
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
  for (const key of used.keys()) consumed.add(key);
  const parts: {
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
    const extentStart = start;
    const inherited =
      parent &&
      ((offset: number, count: number) => parent(extentStart + offset, count));
    const image =
      extent.kind === "SPARSE"
        ? await openVmdk(part, undefined, {
            size: extent.size,
            budget,
            parent: inherited,
          })
        : extent.kind === "VMFSSPARSE" || extent.kind === "SESPARSE"
          ? await openEsxiSparse(
              part,
              extent.kind,
              extent.size,
              budget,
              inherited,
            )
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
  const stream = budget.stream;
  const captured: { file: ImageFile; at: number; data: Uint8Array }[] = [];
  return {
    size: desc.size,
    sectorSize: 512,
    fileCount: used.size + 1,
    format:
      desc.type === "vmfs"
        ? "VMDK · VMFS"
        : desc.type === "vmfssparse" || desc.type === "sesparse"
          ? parts[0].image!.format
          : desc.type === "monolithicflat"
            ? "VMDK · 平面数据"
            : desc.type === "twogbmaxextentflat"
              ? "VMDK · 分卷平面"
              : "VMDK · 分卷稀疏",
    warnings: [...new Set(parts.flatMap((p) => p.image?.warnings ?? []))],
    notes: [
      `已按描述文件顺序组合 ${desc.extents.length} 个数据范围，仅使用明确选择的文件。`,
      "VMDK 没有完整内容校验和；同名文件匹配和结构检查不能证明数据文件属于同一副本，请选择同一离线镜像的完整文件组。",
      ...new Set(parts.flatMap((p) => p.image?.notes ?? [])),
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

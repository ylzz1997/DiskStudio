import { invoke } from "@tauri-apps/api/core";
import { MissingImageFilesError } from "./imageDependencies";
import {
  logicalLimit,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { openParallels } from "./parallelsImage";

const MiB = 1024 ** 2;
export const isParallelsDescriptor = (name: string) =>
  name.toLowerCase() === "diskdescriptor.xml";

export interface ParallelsDescriptor {
  size: number;
  top: string;
  snapshots: ParallelsSnapshot[];
  storages: {
    start: number;
    end: number;
    cluster: number;
    layers: { id: string; name: string; sparse: boolean }[];
  }[];
}

export interface ParallelsSnapshot {
  id: string;
  parent: string | null;
  name: string;
  supported: boolean;
}

const nameKey = (name: string) => name.normalize("NFC").toLowerCase();
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((value, i) => value === b[i]);

async function readFile(file: ImageFile, at: number, length: number) {
  const result = new Uint8Array(
    await file.slice(at, at + length).arrayBuffer(),
  );
  if (result.length !== length) throw Error("Parallels 文件读取不完整。");
  return result;
}

async function descriptorBytes(file: ImageFile) {
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < 1 ||
    file.size > 256 * 1024
  )
    throw Error("Parallels 描述文件须在 256 KiB 以内。");
  return readFile(file, 0, file.size);
}

const parseDescriptor = (bytes: Uint8Array) =>
  invoke<ParallelsDescriptor>("image_parallels_descriptor", bytes);

export async function listParallelsSnapshots(file: ImageFile) {
  const { top, snapshots } = await parseDescriptor(await descriptorBytes(file));
  return { top, snapshots };
}

export async function openParallelsBundle(
  descriptor: ImageFile,
  companions: ImageFile[],
  stream: ImageReadMode,
  snapshot?: string,
): Promise<LogicalImage> {
  if (companions.length > 63)
    throw Error("Parallels 描述文件和数据分卷合计最多 64 个。");
  const selected = new Map<string, ImageFile>();
  for (const companion of companions) {
    const key = nameKey(companion.name);
    if (isParallelsDescriptor(key) || selected.has(key))
      throw Error("所选 Parallels 文件名重复，无法唯一匹配分卷。");
    selected.set(key, companion);
  }

  const xml = await descriptorBytes(descriptor);
  const {
    size,
    top,
    snapshots,
    storages: catalogue,
  } = await parseDescriptor(xml);
  const byId = new Map(snapshots.map((entry) => [entry.id, entry]));
  const point = byId.get(snapshot ?? top);
  if (!point) throw Error("所选 Parallels 快照不在此描述文件中。");
  if (!point.supported)
    throw Error("所选 Parallels 快照需要超过 16 层或 64 个文件。");
  const ids = [point.id];
  let parent = point.parent;
  while (parent) {
    const previous = byId.get(parent);
    if (!previous || ids.length >= 16 || ids.includes(parent))
      throw Error("Parallels 快照标识或父子关系不完整、重复或存在循环。");
    ids.push(parent);
    parent = previous.parent;
  }
  const storages = catalogue.map(({ layers, ...extent }) => {
    const byId = new Map(layers.map((layer) => [layer.id, layer]));
    return {
      ...extent,
      layers: ids.map((id) => {
        const layer = byId.get(id);
        if (!layer) throw Error("Parallels 分卷缺少快照层。");
        return layer;
      }),
    };
  });
  const required = new Set<string>();
  const missing: string[] = [];
  let fileSize = descriptor.size;
  for (const { layers, start, end } of storages) {
    for (const layer of layers) {
      const key = nameKey(layer.name);
      if (required.has(key))
        throw Error("Parallels 描述文件含重复的数据文件名或快照标识。");
      required.add(key);
      const file = selected.get(key);
      if (!file) {
        missing.push(layer.name);
        continue;
      }
      if (
        !Number.isSafeInteger(file.size) ||
        file.size < 512 ||
        file.size % 512
      )
        throw Error("Parallels 数据分卷文件长度无效。");
      if (!layer.sparse && file.size !== end - start)
        throw Error("Parallels 平面基盘的文件长度与分卷容量不符。");
      fileSize += file.size;
    }
  }
  if (missing.length) throw new MissingImageFilesError(missing);
  if (!Number.isSafeInteger(fileSize))
    throw Error("Parallels 文件总长度超出安全范围。");

  let mappedBytes = 0;
  const reserveMetadata = (bytes: number) => {
    mappedBytes += bytes;
    if (mappedBytes > 32 * MiB)
      throw Error("Parallels 目录包的块映射与扩展元数据合计超过 32 MiB。");
  };
  const views: { start: number; end: number; read: LogicalImage["read"] }[] =
    [];
  const images: LogicalImage[] = [];
  const flatReads: { file: ImageFile; at: number; bytes: Uint8Array }[] = [];
  for (const { start, end, cluster, layers } of storages) {
    let read: LogicalImage["read"] | undefined;
    for (const layer of [...layers].reverse()) {
      const file = selected.get(nameKey(layer.name))!;
      if (layer.sparse) {
        const image = await openParallels(file, stream, {
          size: end - start,
          cluster,
          reserveMetadata,
          parent: read,
        });
        images.push(image);
        read = image.read;
      } else {
        read = async (at, length) => {
          const bytes = await readFile(file, at, length);
          if (!stream) flatReads.push({ file, at, bytes: bytes.slice() });
          return bytes;
        };
      }
    }
    if (!read) throw Error("Parallels 分卷缺少快照层。");
    views.push({ start, end, read });
  }
  const chain = storages[0].layers.map((_, i) =>
    storages.map(({ layers }) => layers[i].name).join(" · "),
  );
  const order = "读取顺序：" + chain.join(" → ") + "。";
  const orderedNote =
    new TextEncoder().encode(order).length <= 4096 ? [order] : [];
  let logicalRead = 0;
  return {
    size,
    format: "Parallels HDD · 目录包快照",
    fileSize,
    fileCount: required.size + 1,
    chain,
    parallelsSnapshot: {
      id: point.id,
      name: point.name,
      current: point.id === top,
    },
    sectorSize: 512,
    warnings: [...new Set(images.flatMap((image) => image.warnings))],
    notes: [
      ...orderedNote,
      "已核对 Parallels 所选快照的父子关系、分卷范围和块大小。",
      "Parallels 分卷没有完整内容校验和，请选择同一离线目录包的原始文件。",
      ...new Set(images.flatMap((image) => image.notes)),
      ...(selected.size > required.size
        ? ["未使用所选文件中的无关文件，未读取其内容。"]
        : []),
    ],
    async read(at, length) {
      if (
        !Number.isSafeInteger(at) ||
        !Number.isSafeInteger(length) ||
        at < 0 ||
        length < 1 ||
        length > MiB ||
        at > size - length
      )
        throw Error("Parallels HDD 读取范围越界。");
      logicalRead += length;
      if (logicalRead > logicalLimit(stream, size))
        throw Error("Parallels 目录包读取超过本次操作预算。");
      const out = new Uint8Array(length);
      for (const view of views) {
        const begin = Math.max(view.start, at);
        const end = Math.min(view.end, at + length);
        if (end > begin)
          out.set(await view.read(begin - view.start, end - begin), begin - at);
      }
      return out;
    },
    async verify() {
      if (!same(xml, await readFile(descriptor, 0, descriptor.size)))
        throw Error(
          "读取期间 Parallels 描述文件发生变化，请重新打开离线副本。",
        );
      for (const image of images) await image.verify();
      for (const item of flatReads) {
        if (
          !same(
            item.bytes,
            await readFile(item.file, item.at, item.bytes.length),
          )
        )
          throw Error(
            "读取期间 Parallels 平面基盘发生变化，请重新打开离线副本。",
          );
      }
    },
  };
}

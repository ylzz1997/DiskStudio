import { invoke } from "@tauri-apps/api/core";
import { openVhdChain } from "./vhdChain";
import { openVhdx } from "./vhdxImage";
import { openVdi } from "./vdiImage";
import { openVmdkSource } from "./vmdkBundle";
import { openParallels } from "./parallelsImage";
import type { ImageFile, ImageReadMode, LogicalImage } from "./imageSource";

// Conversion uses two complete source passes; browsing uses fresh random-read
// budgets for each operation. Inventory keeps its smaller inspection quotas.
export async function openContainer(
  file: ImageFile,
  companions: ImageFile[],
  stream: ImageReadMode = false,
): Promise<LogicalImage | null> {
  if (/\.a?vhd$/i.test(file.name))
    return openVhdChain(file, companions, stream);
  if (/\.vhdx$/i.test(file.name)) return openVhdx(file, stream);
  if (/\.vdi$/i.test(file.name)) return openVdi(file, stream);
  if (/\.vmdk$/i.test(file.name))
    return openVmdkSource(
      file,
      companions,
      async (bytes, expected) =>
        new Uint8Array(
          await invoke<number[]>("image_inflate_grain", {
            bytes: Array.from(bytes),
            expected,
          }),
        ),
      stream,
    );
  if (/\.(hdd|hds)$/i.test(file.name)) return openParallels(file, stream);
  return null;
}
export interface ImageFiles {
  file: File;
  companions: File[];
  sector: 512 | 4096;
}
const selected = new Map<string, ImageFiles>();
export function retainImageFiles(id: string, files: ImageFiles) {
  selected.set(id, files);
}
export function imageFiles(id: string) {
  return selected.get(id);
}
export function releaseImageFiles(id: string) {
  selected.delete(id);
}

export async function conversionReader(
  files: ImageFiles,
): Promise<LogicalImage> {
  const { file, companions } = files;
  if (
    !/\.(img|raw|vhd|avhd|vhdx|vdi|vmdk|hdd|hds)$/i.test(file.name) ||
    file.name.length > 255 ||
    /[\\/:\0]/.test(file.name)
  )
    throw Error("请选择普通的受支持镜像文件。");
  const container = await openContainer(file, companions, true);
  if (container) return container;
  if (
    !Number.isSafeInteger(file.size) ||
    file.size < 4 * 1024 ** 2 ||
    file.size > 2 ** 43 ||
    file.size % files.sector
  )
    throw Error("RAW 镜像容量须为 4 MiB–8 TiB，并按所选扇区对齐。");
  let next = 0;
  return {
    size: file.size,
    sectorSize: files.sector,
    format: "RAW · 原始镜像",
    warnings: [],
    notes: [],
    async verify() {},
    async read(at, length) {
      if (
        at !== next ||
        !Number.isSafeInteger(length) ||
        length < 1 ||
        length > 1024 ** 2 ||
        at > file.size - length
      )
        throw Error("RAW 转换只接受有界的连续读取。");
      const b = new Uint8Array(await file.slice(at, at + length).arrayBuffer());
      if (b.length !== length) throw Error("源镜像读取不完整。");
      next += length;
      return b;
    },
  };
}

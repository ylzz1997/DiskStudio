import type { LogicalImage } from "./imageSource";
export type ImageOutputFormat =
  | "raw"
  | "vhdFixed"
  | "vhdDynamic"
  | "vdiDynamic"
  | "vdiFixed"
  | "vhdxDynamic"
  | "vhdxFixed"
  | "vmdkSparse"
  | "parallels";
export interface ImageConversionReport {
  id: string;
  options: { format: ImageOutputFormat; size: number; sector: number };
  state: string;
  message: string;
  received: number;
  sourceVerified: number;
  verified: number;
  outputName: string;
  partialName: string;
  creation?: { table: "none" | "gpt" | "mbr"; diskId: string | null } | null;
  result: null | {
    logicalBytes: number;
    fileBytes: number;
    allocatedBlocks: number;
    sha256: string;
  };
}
export interface ConversionPort {
  begin(
    options: ImageConversionReport["options"],
  ): Promise<ImageConversionReport | null>;
  push(
    id: string,
    offset: number,
    bytes: Uint8Array,
    check: boolean,
  ): Promise<number>;
  finish(id: string): Promise<ImageConversionReport>;
  cancel(id: string): Promise<unknown>;
}
// Matches image_conversion_wire.rs. Binary IPC avoids expanding every source
// byte into a JSON number; the native task still enforces exact sequencing.
export function conversionFrame(
  id: string,
  offset: number,
  bytes: Uint8Array,
  check: boolean,
) {
  if (
    !/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(id) ||
    !Number.isSafeInteger(offset) ||
    offset < 0 ||
    offset >= 2 ** 43 ||
    offset % 1024 ** 2 ||
    bytes.length < 1 ||
    bytes.length > 1024 ** 2
  )
    throw Error("转换数据帧无效。");
  const frame = new Uint8Array(25 + bytes.length),
    hex = id.replaceAll("-", "");
  frame[0] = check ? 1 : 0;
  for (let i = 0; i < 16; i++)
    frame[1 + i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  if (!frame.subarray(1, 17).some(Boolean)) throw Error("转换任务标识为空。");
  new DataView(frame.buffer).setBigUint64(17, BigInt(offset), true);
  frame.set(bytes, 25);
  return frame;
}
export const conversionFormats: {
  value: ImageOutputFormat;
  label: string;
  detail: string;
}[] = [
  {
    value: "raw",
    label: "RAW 原始镜像",
    detail: "连续保存全部逻辑内容，支持 512 B 和 4 KiB 扇区。",
  },
  {
    value: "vhdFixed",
    label: "VHD 固定容量",
    detail: "完整分配逻辑容量，512 B 扇区，最大 2040 GiB。",
  },
  {
    value: "vhdDynamic",
    label: "VHD 动态扩展",
    detail: "全零块不占用数据区，512 B 扇区，最大 2040 GiB。",
  },
  {
    value: "vdiDynamic",
    label: "VDI 动态扩展",
    detail: "独立 VirtualBox 镜像，全零块不分配，512 B 扇区。",
  },
  {
    value: "vdiFixed",
    label: "VDI 固定容量",
    detail: "独立 VirtualBox 镜像，完整分配全部数据块，512 B 扇区。",
  },
  {
    value: "vhdxDynamic",
    label: "VHDX 动态扩展",
    detail: "独立 VHDX 镜像，全零块不分配，保留 512 B 或 4 KiB 扇区。",
  },
  {
    value: "vhdxFixed",
    label: "VHDX 固定容量",
    detail: "完整分配数据块，保留 512 B 或 4 KiB 扇区。",
  },
  {
    value: "vmdkSparse",
    label: "VMDK 单文件稀疏",
    detail:
      "独立 monolithicSparse 镜像，含冗余映射表，512 B 扇区，最大 2040 GiB。",
  },
  {
    value: "parallels",
    label: "Parallels HDS 扩展结构",
    detail: "独立 HDS 数据文件，512 B 扇区；不创建虚拟机目录包或快照描述文件。",
  },
];
export function conversionOptions(
  source: LogicalImage,
  format: ImageOutputFormat,
) {
  return imageOutputOptions(source.size, source.sectorSize, format);
}
export function imageOutputOptions(
  size: number,
  sector: number,
  format: ImageOutputFormat,
) {
  if (
    !Number.isSafeInteger(size) ||
    size < 4 * 1024 ** 2 ||
    size > 2 ** 43 ||
    ![512, 4096].includes(sector) ||
    size % sector
  )
    throw Error("镜像容量须为 4 MiB–8 TiB，并按所选逻辑扇区对齐。");
  if (!["raw", "vhdxDynamic", "vhdxFixed"].includes(format) && sector !== 512)
    throw Error("该目标格式仅支持 512 B 扇区，请为 4 KiB 源选择 RAW 或 VHDX。");
  if (
    (format === "vhdFixed" || format === "vhdDynamic") &&
    size > 2040 * 1024 ** 3
  )
    throw Error("VHD v1 不能超过 2040 GiB。");
  if (format === "vmdkSparse" && size > 2040 * 1024 ** 3)
    throw Error("单文件稀疏 VMDK 不能超过 2040 GiB。");
  return { format, size, sector };
}
// A single bounded request is in flight. Native code independently enforces
// offsets, sizes, both full source digests and destination readback before rename.
export async function convertImage(
  open: () => Promise<LogicalImage>,
  format: ImageOutputFormat,
  port: ConversionPort,
  signal: AbortSignal,
  started: (r: ImageConversionReport) => void,
) {
  const check = () => {
    if (signal.aborted) throw Error("转换已停止。");
  };
  check();
  const source = await open();
  check();
  await source.verify();
  const options = conversionOptions(source, format);
  check();
  const job = await port.begin(options);
  if (!job) return null;
  started(job);
  try {
    for (const verifying of [false, true]) {
      check();
      const reader = verifying ? await open() : source;
      if (
        reader.size !== source.size ||
        reader.sectorSize !== source.sectorSize ||
        reader.format !== source.format ||
        reader.parallelsSnapshot?.id !== source.parallelsSnapshot?.id
      )
        throw Error("重新读取时源镜像类型、容量、扇区大小或快照发生变化。");
      await reader.verify();
      for (let at = 0; at < reader.size; at += 1024 ** 2) {
        check();
        const length = Math.min(1024 ** 2, reader.size - at);
        const b = await reader.read(at, length);
        check();
        if (b.length !== length) throw Error("源镜像块读取不完整。");
        const next = await port.push(job.id, at, b, verifying);
        if (next !== at + length) throw Error("转换接收进度不一致。");
      }
      check();
      await reader.verify();
    }
    check();
    return await port.finish(job.id);
  } catch (e) {
    await port.cancel(job.id).catch(() => {});
    throw e;
  }
}

import type { FormatOptions, WindowsFileSystem } from "./types";

export const defaultFormatOptions: FormatOptions = {
  mode: "quick",
  ntfsCompression: false,
  ntfsShortNames: false,
};
export const formatClusters: Record<WindowsFileSystem, number[]> = {
  NTFS: [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536],
  FAT12: [512, 1024, 2048, 4096, 8192, 16384, 32768],
  FAT16: [512, 1024, 2048, 4096, 8192, 16384, 32768],
  FAT32: [512, 1024, 2048, 4096, 8192, 16384, 32768],
  exFAT: [
    4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152,
    4194304, 8388608, 16777216, 33554432,
  ],
};
export function effectiveFormatCluster(
  fs: WindowsFileSystem,
  cluster: number,
  options: FormatOptions,
) {
  return fs === "NTFS" && options.ntfsCompression && cluster === 0
    ? 4096
    : cluster;
}
export function validateFormatOptions(
  fs: WindowsFileSystem,
  cluster: number,
  sector: number,
  options: FormatOptions,
) {
  if (options.mode !== "quick" && options.mode !== "full")
    throw new Error("格式化方式无效。");
  if (fs !== "NTFS" && (options.ntfsCompression || options.ntfsShortNames))
    throw new Error("压缩和 8.3 短文件名选项仅适用于 NTFS。");
  const effective = effectiveFormatCluster(fs, cluster, options);
  if (options.ntfsCompression && effective > 4096)
    throw new Error("NTFS 压缩需要不大于 4 KiB 的簇。");
  if (
    ![512, 4096].includes(sector) ||
    (effective !== 0 &&
      (!formatClusters[fs].includes(effective) || effective < sector))
  )
    throw new Error("簇大小或逻辑扇区信息无效。");
}

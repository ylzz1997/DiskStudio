import { invoke, isTauri } from "@tauri-apps/api/core";
import { openContainer, retainImageFiles } from "./imageReader";
import type {
  Disk,
  DiskSnapshot,
  PartitionAction,
  PartitionPlan,
  PartitionStatus,
  PartitionTarget,
  TableReport,
} from "./types";
export const native = isTauri();
export class ImageInspectionError extends Error {
  constructor(
    message: string,
    public report: TableReport,
  ) {
    super(message);
  }
}
export async function readPartitionStatus(): Promise<PartitionStatus> {
  return native
    ? invoke<PartitionStatus>("partitions_status")
    : { enabled: false, reason: null, targets: [] };
}
export async function preparePartition(
  target: PartitionTarget,
  action: PartitionAction,
): Promise<PartitionPlan> {
  if (!native) throw new Error("真实分区操作在 Windows 桌面版中提供。");
  return invoke<PartitionPlan>("partitions_prepare", {
    request: { target: target.target, revision: target.revision, action },
  });
}
export async function executePartition(
  token: string,
): Promise<{ message: string; verified: boolean }> {
  if (!native) throw new Error("浏览器不执行分区操作。");
  return invoke("partitions_execute", { token });
}
export async function readLocalDisks(): Promise<DiskSnapshot> {
  if (!native) throw new Error("本机磁盘信息在 Windows 桌面版中提供。");
  return invoke<DiskSnapshot>("local_disks");
}
export async function openImage(
  file: File,
  companions: File[] = [],
): Promise<Disk> {
  if (!/\.(img|raw|vhd|avhd|vhdx|vdi|vmdk|hdd|hds)$/i.test(file.name))
    throw new Error(
      "请选择 RAW、IMG、VHD/AVHD、VHDX、VDI、VMDK 或 Parallels HDD/HDS。",
    );
  if (!native) throw new Error("请在桌面应用中打开镜像文件。");
  if (file.name.length > 255 || /[\\/:\0]/.test(file.name))
    throw new Error("只接受普通镜像文件名。");
  const container = await openContainer(file, companions);
  const size = container?.size ?? file.size;
  if (!Number.isSafeInteger(size) || size < 4 * 1024 ** 2 || size > 2 ** 43)
    throw new Error("镜像容量须介于 4 MiB 与 8 TiB 之间。");
  const [head, tail] = await Promise.all([
    container
      ? container.read(0, 1024 ** 2)
      : file.slice(0, 1024 ** 2).arrayBuffer(),
    container
      ? container.read(size - 1024 ** 2, 1024 ** 2)
      : file.slice(size - 1024 ** 2).arrayBuffer(),
  ]);
  const headBytes = head instanceof Uint8Array ? head : new Uint8Array(head);
  const tailBytes = tail instanceof Uint8Array ? tail : new Uint8Array(tail);
  const logicalSector = container?.sectorSize ?? 512;
  const geometry = container ? { sector: logicalSector } : {};
  const hasGpt = (offset: number) =>
    [..."EFI PART"].every(
      (c, i) => headBytes[offset + i] === c.charCodeAt(0),
    );
  if (container && hasGpt(logicalSector === 512 ? 4096 : 512))
    throw new Error("容器声明的逻辑扇区大小与 GPT 位置冲突。");
  const ebrs: { offset: number; bytes: number[] }[] = [];
  const mbr = Array.from(headBytes.subarray(0, logicalSector));
  for (; !(logicalSector === 4096 && hasGpt(4096));) {
    const offset = await invoke<number | null>("image_ebr_next", {
      mbr,
      size,
      ebrs,
      ...geometry,
    });
    if (offset === null) break;
    if (
      !Number.isSafeInteger(offset) ||
      offset < logicalSector ||
      offset % logicalSector !== 0 ||
      offset + logicalSector > size ||
      ebrs.length >= 64 ||
      ebrs.some((sector) => sector.offset === offset)
    )
      throw new Error("EBR 读取计划越界、重复或超过上限。");
    // Slice only the File selected by the user. No paths enter native commands.
    const bytes = Array.from(
      container
        ? await container.read(offset, logicalSector)
        : new Uint8Array(await file.slice(offset, offset + logicalSector).arrayBuffer()),
    );
    if (bytes.length !== logicalSector)
      throw new Error("EBR 扇区读取不完整，请重新打开镜像。");
    ebrs.push({ offset, bytes });
  }
  await container?.verify();
  const result = await invoke<{
    disk: Disk | null;
    parseError: string | null;
    report: TableReport;
  }>("inspect_image", {
    head: Array.from(headBytes),
    tail: Array.from(tailBytes),
    size,
    filename: file.name,
    ebrs,
    ...geometry,
  });
  if (container) {
    const warning = container.warnings.length > 0;
    result.report = {
      ...result.report,
      state:
        warning && result.report.state === "passed"
          ? "warning"
          : result.report.state,
      items: [
        {
          code: "virtual-container",
          title: "虚拟磁盘容器",
          state: warning ? "warning" : "passed",
          detail: [
            container.format,
            "已核对容器元数据与块映射范围；未校验全部数据内容。",
            ...container.warnings,
          ].join(" "),
        },
        ...result.report.items,
      ],
    };
  }
  if (!result.disk)
    throw new ImageInspectionError(
      result.parseError ?? "分区表无法解析。",
      result.report,
    );
  const disk = result.disk;
  if (disk.kind !== "image") throw new Error("镜像解析结果无效。");
  if (container && disk.sectorSize !== container.sectorSize)
    throw new Error("分区表扇区大小与虚拟磁盘元数据不一致。");
  retainImageFiles(disk.id, {
    file,
    companions,
    sector: disk.sectorSize as 512 | 4096,
  });
  return {
    ...disk,
    tableReport: result.report,
    imageFormat: container?.format ?? "RAW · 原始镜像",
    imageFileSize: container?.fileSize ?? file.size,
    imageFileCount: container?.fileCount ?? 1,
    imageChain: container?.chain,
    imageNotes: container ? [...container.warnings, ...container.notes] : [],
  };
}

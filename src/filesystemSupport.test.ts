import { expect, it } from "vitest";
import { selectedFilesystemSupport } from "./filesystemSupport";
import type { Disk, Partition, PartitionTarget } from "./types";

const disk = {
  id: "disk",
  hardware: { identity: "id", layoutKey: "layout", isSystem: true },
} as Disk;
const partition = { number: 2, fs: "未知" } as Partition;
const target = {
  diskId: "disk",
  target: "id",
  layoutKey: "layout",
  available: true,
  formatPartitions: [3],
  resizePartitions: [3],
  extPartitions: [2],
} as PartitionTarget;

it("does not infer Ext from a Linux type or allow a stale capability snapshot", () => {
  const info = selectedFilesystemSupport(disk, partition, target);
  expect(info.filesystem).toBe("未知");
  expect(info.resize).toContain("需显式检查");
  for (const change of [
    { target: "replacement" },
    { layoutKey: "new" },
    { diskId: "other" },
    { available: false },
  ]) {
    expect(
      selectedFilesystemSupport(disk, partition, { ...target, ...change })
        .format,
    ).toBe("当前分区不可格式化");
  }
});
it("uses each operation's partition capabilities instead of the disk's Windows status", () => {
  const ntfs = { ...partition, number: 3, fs: "NTFS" };
  expect(selectedFilesystemSupport(disk, ntfs, target).resize).toContain(
    "Windows 检查",
  );
  expect(
    selectedFilesystemSupport(disk, { ...ntfs, fs: "exFAT" }, target).resize,
  ).toBe("当前分区不可调整大小");
  expect(
    selectedFilesystemSupport(disk, ntfs, { ...target, formatPartitions: [] })
      .format,
  ).toBe("当前分区不可格式化");
  expect(
    selectedFilesystemSupport(disk, ntfs, { ...target, formatPartitions: [] })
      .resize,
  ).toContain("Windows 检查");
});

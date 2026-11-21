import { beforeEach, expect, it, vi } from "vitest";
import type { Disk, TableReport } from "./types";
const bridge = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: bridge.invoke,
}));
import { ImageInspectionError, openImage } from "./backend";
import { MissingVmdkFilesError } from "./vmdkBundle";
import { MissingImageFilesError } from "./imageDependencies";
import { chainBytes, chainFile, stamp } from "../tests/fixtures/vhd-chain-data";
import { bundleFile, bundleNames } from "../tests/fixtures/vmdk-bundle-data";
import { vdiBytes, le32 as vdi32 } from "../tests/fixtures/vdi-memory";
import {
  fixture as parallelsFixture,
  raw as parallelsRaw,
  get32 as parallels32,
} from "../tests/fixtures/parallels-data";
import {
  fixture as vmdkFixture,
  raw as vmdkRaw,
  inflate,
  editDescriptor,
} from "../tests/fixtures/vmdk-data";
import {
  fixture as vhdxFixture,
  BAT,
  get64,
  item,
  le32,
} from "../tests/fixtures/vhdx-data";
import {
  dynamicVhd,
  memoryFile,
  logicalDisk,
} from "../tests/fixtures/vhd-memory";

const MiB = 1024 ** 2;
const report: TableReport = {
  filename: "test.img",
  scheme: "GPT",
  sectorSize: 512,
  state: "passed",
  items: [],
};
const disk: Disk = {
  id: "image-test",
  name: "test",
  filename: "test.img",
  size: 8 * MiB,
  sectorSize: 512,
  scheme: "GPT",
  kind: "image",
  partitions: [],
};
function file(size = 8 * MiB, name = "test.img") {
  const slice = vi.fn((start: number) => ({
    arrayBuffer: async () =>
      new Uint8Array(512).fill(start === 0 ? 1 : 2).buffer,
  }));
  return { value: { size, name, slice } as unknown as File, slice };
}
beforeEach(() => {
  bridge.invoke.mockReset();
  bridge.invoke.mockImplementation(async (command) =>
    command === "image_ebr_next" ? null : { disk, report, parseError: null },
  );
});
it("reads only bounded ends and sends bytes, size and basename to inspection", async () => {
  const input = file();
  const result = await openImage(input.value);
  expect(input.slice.mock.calls).toEqual([[0, MiB], [7 * MiB]]);
  expect(bridge.invoke).toHaveBeenCalledTimes(2);
  expect(bridge.invoke).toHaveBeenLastCalledWith("inspect_image", {
    head: Array(512).fill(1),
    tail: Array(512).fill(2),
    size: 8 * MiB,
    filename: "test.img",
    ebrs: [],
  });
  expect(result.tableReport).toEqual(report);
});
it("retains the diagnostic report when damaged tables cannot be imported", async () => {
  const bad = { ...report, state: "failed" };
  bridge.invoke.mockImplementation(async (command) =>
    command === "image_ebr_next"
      ? null
      : {
          disk: null,
          report: bad,
          parseError: "主表损坏",
        },
  );
  await expect(openImage(file().value)).rejects.toMatchObject({
    message: "主表损坏",
    report: bad,
  });
  await expect(openImage(file().value)).rejects.toBeInstanceOf(
    ImageInspectionError,
  );
});
it("reads only each requested EBR sector from the selected File and includes it in the report", async () => {
  bridge.invoke
    .mockResolvedValueOnce(2 * MiB)
    .mockResolvedValueOnce(4 * MiB)
    .mockResolvedValueOnce(null);
  const input = file();
  await openImage(input.value);
  expect(input.slice.mock.calls).toEqual([
    [0, MiB],
    [7 * MiB],
    [2 * MiB, 2 * MiB + 512],
    [4 * MiB, 4 * MiB + 512],
  ]);
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(
    args.ebrs.map((s: { offset: number; bytes: number[] }) => [
      s.offset,
      s.bytes.length,
    ]),
  ).toEqual([
    [2 * MiB, 512],
    [4 * MiB, 512],
  ]);
  expect(Object.keys(args).sort()).toEqual([
    "ebrs",
    "filename",
    "head",
    "size",
    "tail",
  ]);
});
it("rejects unsafe read offsets before any additional file slice", async () => {
  for (const offset of [-512, 0, 513, 8 * MiB, Number.MAX_SAFE_INTEGER, NaN]) {
    bridge.invoke.mockResolvedValueOnce(offset);
    const input = file();
    await expect(openImage(input.value)).rejects.toThrow("读取计划");
    expect(input.slice).toHaveBeenCalledTimes(2);
  }
});
it("bounds repeated and excessively long EBR read plans", async () => {
  bridge.invoke.mockResolvedValue(2 * MiB);
  const repeated = file();
  await expect(openImage(repeated.value)).rejects.toThrow("重复");
  expect(repeated.slice).toHaveBeenCalledTimes(3);
  let n = 0;
  bridge.invoke.mockImplementation(async () => 2 * MiB + n++ * 512);
  const long = file();
  await expect(openImage(long.value)).rejects.toThrow("上限");
  expect(long.slice).toHaveBeenCalledTimes(66);
});
it("rejects a truncated EBR read without submitting partial bytes", async () => {
  bridge.invoke.mockResolvedValueOnce(2 * MiB);
  const input = file();
  input.slice.mockImplementation((start) => ({
    arrayBuffer: async () =>
      new Uint8Array(start === 2 * MiB ? 511 : 512).buffer,
  }));
  await expect(openImage(input.value)).rejects.toThrow("读取不完整");
  expect(bridge.invoke).toHaveBeenCalledTimes(1);
});
it("rejects container formats and invalid sizes before reading file bytes", async () => {
  for (const input of [file(8 * MiB, "disk.iso"), file(1), file(2 ** 43 + 1)]) {
    await expect(openImage(input.value)).rejects.toThrow();
    expect(input.slice).not.toHaveBeenCalled();
  }
  expect(bridge.invoke).not.toHaveBeenCalled();
});
it("passes decoded logical VHD head, tail and EBR bytes through the same path-free IPC", async () => {
  const raw = logicalDisk();
  bridge.invoke.mockResolvedValueOnce(4 * MiB).mockResolvedValueOnce(null);
  const result = await openImage(memoryFile(dynamicVhd(raw)).file);
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(args.size).toBe(raw.length);
  expect(args.filename).toBe("memory.vhd");
  expect(args.head.length).toBe(MiB);
  expect(args.tail.length).toBe(MiB);
  expect(args.head.every((v: number, i: number) => v === raw[i])).toBe(true);
  expect(
    args.tail.every((v: number, i: number) => v === raw[raw.length - MiB + i]),
  ).toBe(true);
  expect(args.ebrs).toEqual([
    { offset: 4 * MiB, bytes: Array.from(raw.slice(4 * MiB, 4 * MiB + 512)) },
  ]);
  expect(Object.keys(args).sort()).toEqual([
    "ebrs",
    "filename",
    "head",
    "sector",
    "size",
    "tail",
  ]);
  expect(result.imageFormat).toBe("VHD · 动态扩展");
  expect(result.imageFileSize).toBeGreaterThan(raw.length);
});
it("inspects an AVHD chain using only merged logical bytes and the current basename", async () => {
  const expected = chainBytes("expected.raw");
  let ebr = 0;
  bridge.invoke.mockImplementation(async (command) =>
    command === "image_ebr_next"
      ? [MiB, 4 * MiB, null][ebr++]
      : { disk, report, parseError: null },
  );
  const result = await openImage(chainFile("leaf.avhd").file, [
    chainFile("base.vhd").file,
    chainFile("middle.avhd").file,
  ]);
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(Object.keys(args).sort()).toEqual([
    "ebrs",
    "filename",
    "head",
    "sector",
    "size",
    "tail",
  ]);
  expect(args.filename).toBe("leaf.avhd");
  expect(args.size).toBe(expected.length);
  expect(args.head.every((v: number, i: number) => v === expected[i])).toBe(
    true,
  );
  expect(
    args.tail.every(
      (v: number, i: number) => v === expected[expected.length - MiB + i],
    ),
  ).toBe(true);
  for (const e of args.ebrs)
    expect(
      e.bytes.every((v: number, i: number) => v === expected[e.offset + i]),
    ).toBe(true);
  expect(result.imageChain).toEqual(["leaf.avhd", "middle.avhd", "base.vhd"]);
  expect(result.imageFileCount).toBe(3);
  expect(result.imageFormat).toBe("VHD · 差分链");
});
it("stops AVHD inspection before IPC when a parent is missing or modified", async () => {
  await expect(openImage(chainFile("leaf.avhd").file)).rejects.toBeInstanceOf(
    MissingImageFilesError,
  );
  const parent = chainFile("middle.avhd").file;
  Object.assign(parent, { lastModified: stamp + 1000 });
  await expect(
    openImage(chainFile("leaf.avhd").file, [parent]),
  ).rejects.toThrow("修改时间");
  expect(bridge.invoke).not.toHaveBeenCalled();
});
it("rejects an invalid VHD before invoking any native command", async () => {
  const bytes = dynamicVhd();
  bytes[bytes.length - 512] ^= 1;
  await expect(openImage(memoryFile(bytes).file)).rejects.toThrow();
  expect(bridge.invoke).not.toHaveBeenCalled();
});
it("imports VHDX 4Kn GPT with container sector size and never asks for 512-byte EBRs", async () => {
  bridge.invoke.mockResolvedValue({
    disk: { ...disk, sectorSize: 4096 },
    report,
    parseError: null,
  });
  const result = await openImage(
    memoryFile(vhdxFixture("gpt4k"), "test.vhdx").file,
  );
  expect(bridge.invoke).toHaveBeenCalledTimes(1);
  const [command, args] = bridge.invoke.mock.calls[0];
  expect(command).toBe("inspect_image");
  expect(args.ebrs).toEqual([]);
  expect(args.filename).toBe("test.vhdx");
  expect(args.sector).toBe(4096);
  expect(args.size).toBe(8 * MiB);
  expect(String.fromCharCode(...args.head.slice(4096, 4104))).toBe("EFI PART");
  expect(result.imageFormat).toBe("VHDX · 动态扩展");
  expect(result.tableReport?.items[0].code).toBe("virtual-container");
});
it("reads a VHDX 4Kn MBR and passes a complete logical EBR sector to inspection", async () => {
  const b = vhdxFixture("dynamic");
  le32(b, item(b, 3), 4096);
  le32(b, item(b, 4), 4096);
  const start = Number(get64(b, BAT) >> 20n) * MiB;
  b.fill(0, start, start + 8192);
  b[start + 450] = 0x0f;
  le32(b, start + 454, 1);
  le32(b, start + 458, 256);
  b.set([0x55, 0xaa], start + 510);
  const ebrStart = start + 4096;
  b[ebrStart + 450] = 7;
  le32(b, ebrStart + 454, 1);
  le32(b, ebrStart + 458, 128);
  b.set([0x55, 0xaa], ebrStart + 510);
  b.fill(0x79, ebrStart + 512, ebrStart + 4096);
  bridge.invoke.mockResolvedValueOnce(4096).mockResolvedValueOnce(null);
  bridge.invoke.mockResolvedValue({
    disk: { ...disk, scheme: "MBR", sectorSize: 4096 }, report, parseError: null,
  });
  const opened = await openImage(memoryFile(b, "mbr.vhdx").file);
  expect(opened.sectorSize).toBe(4096);
  const [plan, request] = bridge.invoke.mock.calls[0];
  expect(plan).toBe("image_ebr_next");
  expect(request.sector).toBe(4096);
  expect(request.mbr).toHaveLength(4096);
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(args.sector).toBe(4096);
  expect(args.ebrs).toEqual([{ offset: 4096, bytes: Array.from(b.slice(ebrStart, ebrStart + 4096)) }]);
});
it("rejects GPT/container disagreement and native-result sector mismatch", async () => {
  const c = vhdxFixture("gpt4k");
  le32(c, item(c, 3), 512);
  await expect(openImage(memoryFile(c, "test.vhdx").file)).rejects.toThrow(
    "冲突",
  );
  expect(bridge.invoke).not.toHaveBeenCalled();
  bridge.invoke.mockResolvedValue({ disk, report, parseError: null });
  await expect(
    openImage(memoryFile(vhdxFixture("gpt4k"), "test.vhdx").file),
  ).rejects.toThrow("不一致");
});
it("retains VHDX single-header warning in the partition report and details", async () => {
  const b = vhdxFixture();
  b[64 * 1024] ^= 1;
  const result = await openImage(memoryFile(b, "test.vhdx").file);
  expect(result.tableReport?.state).toBe("warning");
  expect(result.tableReport?.items[0]).toMatchObject({
    code: "virtual-container",
    state: "warning",
  });
  expect(result.imageNotes?.[0]).toContain("只有一份");
});
it("imports decoded VDI logical windows and EBRs without adding any path IPC", async () => {
  const raw = logicalDisk();
  bridge.invoke.mockResolvedValueOnce(4 * MiB).mockResolvedValueOnce(null);
  const result = await openImage(
    memoryFile(vdiBytes(raw), "备份.磁盘.VDI").file,
  );
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(Object.keys(args).sort()).toEqual([
    "ebrs",
    "filename",
    "head",
    "sector",
    "size",
    "tail",
  ]);
  expect(args.filename).toBe("备份.磁盘.VDI");
  expect(args.size).toBe(raw.length);
  expect(args.head.every((v: number, i: number) => v === raw[i])).toBe(true);
  expect(
    args.tail.every((v: number, i: number) => v === raw[raw.length - MiB + i]),
  ).toBe(true);
  expect(args.ebrs).toEqual([
    { offset: 4 * MiB, bytes: Array.from(raw.slice(4 * MiB, 4 * MiB + 512)) },
  ]);
  expect(result.imageFormat).toBe("VDI · 动态扩展");
  expect(result.imageNotes?.[0]).toContain("不含校验和");
  expect(result.tableReport?.items[0].code).toBe("virtual-container");
});
it("rejects VDI parents and 4Kn GPT before invoking the native parser", async () => {
  const parent = vdiBytes();
  vdi32(parent, 424, 1);
  await expect(
    openImage(memoryFile(parent, "parent.vdi").file),
  ).rejects.toThrow("父盘");
  const raw = logicalDisk();
  raw.set(new TextEncoder().encode("EFI PART"), 4096);
  await expect(
    openImage(memoryFile(vdiBytes(raw), "conflict.vdi").file),
  ).rejects.toThrow("冲突");
  expect(bridge.invoke).not.toHaveBeenCalled();
});
it("retains VDI ignored-tail warning in the report instead of silently showing passed", async () => {
  const bytes = vdiBytes(),
    appended = new Uint8Array(bytes.length + 512);
  appended.set(bytes);
  appended.fill(0xa5, bytes.length);
  const result = await openImage(memoryFile(appended, "trailing.vdi").file);
  expect(result.tableReport?.state).toBe("warning");
  expect(result.tableReport?.items[0].detail).toContain("额外字节");
  expect(result.imageNotes?.some((n) => n.includes("额外字节"))).toBe(true);
});

it("imports multipart VMDK with exact logical bytes and no companion filenames or paths in IPC", async () => {
  const expected = vmdkRaw();
  for (const names of [bundleNames.sparse, bundleNames.split]) {
    let ebr = 0;
    bridge.invoke.mockReset();
    bridge.invoke.mockImplementation(async (command) =>
      command === "image_ebr_next"
        ? [MiB, 4 * MiB, null][ebr++]
        : { disk, report, parseError: null },
    );
    const result = await openImage(
      bundleFile(names[0]),
      names.slice(1).map(bundleFile),
    );
    const [command, args] = bridge.invoke.mock.calls.at(-1)!;
    expect(command).toBe("inspect_image");
    expect(Object.keys(args).sort()).toEqual([
      "ebrs",
      "filename",
      "head",
      "sector",
      "size",
      "tail",
    ]);
    expect(args.filename).toBe(names[0]);
    expect(args.size).toBe(expected.length);
    expect(args.head.every((v: number, i: number) => v === expected[i])).toBe(
      true,
    );
    expect(
      args.tail.every(
        (v: number, i: number) => v === expected[expected.length - MiB + i],
      ),
    ).toBe(true);
    for (const e of args.ebrs)
      expect(
        e.bytes.every((v: number, i: number) => v === expected[e.offset + i]),
      ).toBe(true);
    expect(result.imageFileCount).toBe(3);
    expect(result.imageFormat).toContain("分卷");
  }
});
it("asks for missing VMDK companions before invoking native inspection", async () => {
  await expect(
    openImage(bundleFile("split-flat.vmdk"), [bundleFile("flat0.bin")]),
  ).rejects.toBeInstanceOf(MissingVmdkFilesError);
  expect(bridge.invoke).not.toHaveBeenCalled();
});
it("imports compressed VMDK head/tail/EBRs with only bounded byte decompression IPC", async () => {
  const raw = vmdkRaw();
  let ebr = 0;
  bridge.invoke.mockImplementation(async (command, args) => {
    if (command === "image_inflate_grain") {
      expect(Object.keys(args).sort()).toEqual(["bytes", "expected"]);
      expect(args.bytes.length).toBeLessThanOrEqual(2 * 65536);
      expect(args.expected).toBe(65536);
      return Array.from(
        await inflate(new Uint8Array(args.bytes), args.expected),
      );
    }
    if (command === "image_ebr_next") return [MiB, 4 * MiB, null][ebr++];
    return { disk, report, parseError: null };
  });
  const result = await openImage(
    memoryFile(vmdkFixture("footer"), "压缩磁盘.VMDK").file,
  );
  const [command, args] = bridge.invoke.mock.calls.at(-1)!;
  expect(command).toBe("inspect_image");
  expect(Object.keys(args).sort()).toEqual([
    "ebrs",
    "filename",
    "head",
    "sector",
    "size",
    "tail",
  ]);
  expect(args.filename).toBe("压缩磁盘.VMDK");
  expect(args.head.every((v: number, i: number) => v === raw[i])).toBe(true);
  expect(
    args.tail.every((v: number, i: number) => v === raw[raw.length - MiB + i]),
  ).toBe(true);
  expect(args.ebrs.map((e: { offset: number }) => e.offset)).toEqual([
    MiB,
    4 * MiB,
  ]);
  for (const e of args.ebrs)
    expect(
      e.bytes.every((v: number, i: number) => v === raw[e.offset + i]),
    ).toBe(true);
  expect(result.imageFormat).toBe("VMDK · 压缩流");
  expect(result.imageNotes?.some((n) => n.includes("未验证其他压缩块"))).toBe(
    true,
  );
});
it("rejects VMDK dependencies before IPC and refuses inconsistent native sector results", async () => {
  const b = vmdkFixture();
  editDescriptor(b, (s) =>
    s.replace("parentCID=ffffffff", "parentCID=12345678"),
  );
  await expect(openImage(memoryFile(b, "parent.vmdk").file)).rejects.toThrow(
    "父盘",
  );
  expect(bridge.invoke).not.toHaveBeenCalled();
  bridge.invoke.mockImplementation(async (command) =>
    command === "image_ebr_next"
      ? null
      : { disk: { ...disk, sectorSize: 4096 }, report, parseError: null },
  );
  await expect(
    openImage(memoryFile(vmdkFixture(), "mismatch.vmdk").file),
  ).rejects.toThrow("不一致");
});
it("stops importing compressed VMDK when native validation rejects a damaged stream", async () => {
  bridge.invoke.mockRejectedValue(new Error("VMDK 压缩数据或校验和损坏。"));
  await expect(
    openImage(memoryFile(vmdkFixture("stream"), "damaged.vmdk").file),
  ).rejects.toThrow("校验和");
  expect(
    bridge.invoke.mock.calls.every(
      ([command]) => command === "image_inflate_grain",
    ),
  ).toBe(true);
});

it("imports Parallels HDD/HDS logical data and retains the unknown snapshot dependency warning", async () => {
  const raw = parallelsRaw();
  for (const [kind, filename] of [
    ["extended", "扩展.HDD"],
    ["legacy", "旧版.HDS"],
  ]) {
    bridge.invoke.mockReset();
    let ebr = 0;
    bridge.invoke.mockImplementation(async (command) =>
      command === "image_ebr_next"
        ? [MiB, 4 * MiB, null][ebr++]
        : { disk, report, parseError: null },
    );
    const result = await openImage(
      memoryFile(parallelsFixture(kind), filename).file,
    );
    const [command, args] = bridge.invoke.mock.calls.at(-1)!;
    expect(command).toBe("inspect_image");
    expect(Object.keys(args).sort()).toEqual([
      "ebrs",
      "filename",
      "head",
      "sector",
      "size",
      "tail",
    ]);
    expect(args.filename).toBe(filename);
    expect(args.size).toBe(raw.length);
    expect(args.head.every((v: number, i: number) => v === raw[i])).toBe(true);
    expect(
      args.tail.every(
        (v: number, i: number) => v === raw[raw.length - MiB + i],
      ),
    ).toBe(true);
    expect(args.ebrs.map((e: { offset: number }) => e.offset)).toEqual([
      MiB,
      4 * MiB,
    ]);
    for (const e of args.ebrs)
      expect(
        e.bytes.every((v: number, i: number) => v === raw[e.offset + i]),
      ).toBe(true);
    expect(result.tableReport?.state).toBe("warning");
    expect(result.tableReport?.items[0].detail).toContain("无法确认快照依赖");
    expect(result.imageNotes?.[0]).toContain("无法确认快照依赖");
    expect(result.imageFormat).toContain(
      kind === "legacy" ? "传统结构" : "扩展结构",
    );
  }
});
it("rejects Parallels 4Kn conflicts before IPC and native sector mismatches after parsing", async () => {
  const b = parallelsFixture(),
    at = parallels32(b, 64) * MiB;
  b.set(new TextEncoder().encode("EFI PART"), at + 4096);
  await expect(openImage(memoryFile(b, "conflict.hdd").file)).rejects.toThrow(
    "冲突",
  );
  expect(bridge.invoke).not.toHaveBeenCalled();
  bridge.invoke.mockImplementation(async (command) =>
    command === "image_ebr_next"
      ? null
      : { disk: { ...disk, sectorSize: 4096 }, report, parseError: null },
  );
  await expect(
    openImage(memoryFile(parallelsFixture(), "mismatch.hdd").file),
  ).rejects.toThrow("不一致");
});
it("rechecks Parallels metadata after logical reads and never publishes a changed image", async () => {
  const b = parallelsFixture();
  bridge.invoke.mockImplementation(async (command) => {
    if (command === "image_ebr_next") {
      b[20] ^= 1;
      return null;
    }
    throw Error("Must not reach inspection");
  });
  await expect(openImage(memoryFile(b, "changed.hdd").file)).rejects.toThrow(
    "发生变化",
  );
  expect(bridge.invoke).toHaveBeenCalledTimes(1);
});

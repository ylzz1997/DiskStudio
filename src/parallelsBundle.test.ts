import { Buffer } from "node:buffer";
import { expect, it, vi } from "vitest";
import { browseReader, type NativeImageGroup } from "./imageBrowserSource";
import { conversionReader } from "./imageReader";
import {
  openParallelsBundle,
  type ParallelsDescriptor,
} from "./parallelsBundle";
import { makeHds } from "../tests/fixtures/parallels-bundle";
import { memoryFile, MiB } from "../tests/fixtures/vhd-memory";

const bytes = new TextEncoder().encode("<Parallels_disk_image/>");
const cluster = 65536;
const root = "5f07cb09-ef78-422e-b000-111111111111";
const current = "5fbaabe3-6958-40ff-92a7-860e329aab41";
const parsed: ParallelsDescriptor = {
  size: 6 * MiB,
  top: current,
  snapshots: [
    { id: current, parent: root, name: "left-child.hds", supported: true },
    { id: root, parent: null, name: "left-base.hdd", supported: true },
  ],
  storages: [
    {
      start: 0,
      end: 2 * MiB,
      cluster,
      layers: [
        { id: current, name: "left-child.hds", sparse: true },
        { id: root, name: "left-base.hdd", sparse: false },
      ],
    },
    {
      start: 2 * MiB,
      end: 6 * MiB,
      cluster,
      layers: [
        { id: current, name: "right-child.hds", sparse: true },
        { id: root, name: "right-base.hds", sparse: true },
      ],
    },
  ],
};

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (command: string, input: Uint8Array) => {
    expect(command).toBe("image_parallels_descriptor");
    expect(input).toEqual(bytes);
    return parsed;
  },
}));

function fixture() {
  const descriptor = bytes.slice();
  const left = Buffer.alloc(2 * MiB, 0x41);
  const right = makeHds(
    4 * MiB,
    cluster,
    [[0, Buffer.alloc(cluster, 0x42)]],
    true,
  );
  const upper = makeHds(4 * MiB, cluster, [
    [cluster, Buffer.alloc(cluster, 0x43)],
  ]);
  const lower = makeHds(2 * MiB, cluster, [[cluster, Buffer.alloc(cluster)]]);
  const data: [string, Uint8Array][] = [
    ["DiskDescriptor.xml", descriptor],
    ["right-child.hds", upper],
    ["left-base.hdd", left],
    ["right-base.hds", right],
    ["left-child.hds", lower],
    ["unrelated.hds", new Uint8Array([1])],
  ];
  const files = data.map(([name, content]) => memoryFile(content, name));
  return { data, files, descriptor, left, right, upper };
}

it("uses the same split snapshot for retained browsing and file conversion", async () => {
  const { data, files } = fixture();
  const group: NativeImageGroup = {
    id: "parallels",
    files: data.map(([name, b], i) => ({
      id: String(i),
      name,
      size: b.length,
      lastModified: 1,
    })),
  };
  const read = async (id: string, file: string, at: number, length: number) => {
    expect(id).toBe(group.id);
    expect(file).not.toBe("5");
    return Uint8Array.from(data[Number(file)][1].subarray(at, at + length));
  };
  const expected = Buffer.alloc(parsed.size);
  expected.fill(0x41, 0, 2 * MiB);
  expected.fill(0, cluster, cluster * 2);
  expected.fill(0x42, 2 * MiB, 2 * MiB + cluster);
  expected.fill(0x43, 2 * MiB + cluster, 2 * MiB + cluster * 2);
  const [first, ...others] = files;
  for (const reader of [
    () => browseReader(group, { kind: "disk", sector: 512 }, read),
    () =>
      conversionReader({
        file: first.file,
        companions: others.map(({ file }) => file),
        sector: 512,
      }),
  ]) {
    const image = await reader();
    expect(image.chain).toEqual([
      "left-child.hds · right-child.hds",
      "left-base.hdd · right-base.hds",
    ]);
    expect(image.fileCount).toBe(5);
    expect(image.fileSize).toBe(
      data.slice(0, 5).reduce((n, [, part]) => n + part.length, 0),
    );
    for (let at = 0; at < expected.length;) {
      const length = Math.min(900 * 1024, expected.length - at);
      expect(
        Buffer.from(await image.read(at, length)).equals(
          expected.subarray(at, at + length),
        ),
      ).toBe(true);
      at += length;
    }
    await image.verify();
  }
});

it("reports all missing active members before reading companions and rejects aliases", async () => {
  const { files } = fixture();
  const [descriptor, upper] = files;
  await expect(
    openParallelsBundle(descriptor.file, [upper.file], false),
  ).rejects.toMatchObject({
    missing: ["left-child.hds", "left-base.hdd", "right-base.hds"],
  });
  expect(upper.reads).toEqual([]);
  await expect(
    openParallelsBundle(descriptor.file, [upper.file, upper.file], false),
  ).rejects.toThrow("重复");
});

it("opens historical views without reading any files exclusive to the current snapshot", async () => {
  const { files } = fixture();
  const [descriptor, upper, left, right, lower] = files;
  const image = await conversionReader({
    file: descriptor.file,
    companions: [upper.file, left.file, right.file, lower.file],
    sector: 512,
    parallelsSnapshot: root,
  });
  expect(image.parallelsSnapshot).toEqual({
    id: root,
    name: "left-base.hdd",
    current: false,
  });
  expect(image.chain).toEqual(["left-base.hdd · right-base.hds"]);
  expect(Buffer.from(await image.read(2 * MiB - 4, 8))).toEqual(
    Buffer.from("AAAABBBB"),
  );
  expect((await image.read(cluster, 1))[0]).toBe(0x41);
  await image.verify();
  expect(upper.reads).toEqual([]);
  expect(lower.reads).toEqual([]);
  await expect(
    openParallelsBundle(descriptor.file, [], false, root),
  ).rejects.toMatchObject({
    missing: ["left-base.hdd", "right-base.hds"],
  });
  await expect(
    openParallelsBundle(descriptor.file, [], false, "missing"),
  ).rejects.toThrow("不在此描述文件");
});

it("checks stored geometry and verifies the descriptor, HDS map and inspected plain bytes", async () => {
  const { files, descriptor, left, upper } = fixture();
  const [first, ...companions] = files.map(({ file }) => file);
  const view = await openParallelsBundle(first, companions, false);
  await view.read(0, 512);
  for (const [bytes, index] of [
    [descriptor, 0],
    [left, 0],
    [upper, 68],
  ] as const) {
    bytes[index] ^= 1;
    await expect(view.verify()).rejects.toThrow("发生变化");
    bytes[index] ^= 1;
  }
  new DataView(upper.buffer, upper.byteOffset, upper.byteLength).setUint32(
    28,
    256,
    true,
  );
  await expect(openParallelsBundle(first, companions, false)).rejects.toThrow(
    "块大小与描述文件",
  );
});

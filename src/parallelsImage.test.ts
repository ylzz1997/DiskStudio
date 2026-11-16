import { expect, it } from "vitest";
import { openParallels } from "./parallelsImage";
import { memoryFile, MiB } from "../tests/fixtures/vhd-memory";
import {
  fixture,
  raw,
  get32,
  set32,
  set64,
} from "../tests/fixtures/parallels-data";
it("decodes independent QEMU images and repacked legacy addresses into exact raw head/tail/EBRs", async () => {
  const expected = raw();
  for (const kind of ["extended", "small", "legacy"]) {
    const b = fixture(kind),
      copy = b.slice(),
      input = memoryFile(b),
      image = await openParallels(input.file);
    expect(image.size).toBe(expected.length);
    expect(image.sectorSize).toBe(512);
    expect(image.format).toContain(kind === "legacy" ? "传统结构" : "扩展结构");
    expect(image.warnings.some((n) => n.includes("无法确认快照依赖"))).toBe(
      true,
    );
    for (const [at, length] of [
      [0, MiB],
      [7 * MiB, MiB],
      [MiB, 512],
      [4 * MiB, 512],
      [65536 - 255, 1024],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === expected[at + i]),
      ).toBe(true);
    await image.verify();
    expect(b.every((v, i) => v === copy[i])).toBe(true);
    expect(input.reads.reduce((n, [a, z]) => n + z - a, 0)).toBeLessThan(
      3 * MiB,
    );
  }
});
it("handles sparse boundaries, empty images, non-power-of-two clusters and partial logical tails", async () => {
  const source = fixture(),
    expected = raw();
  const blank = source.slice(0, MiB);
  blank.fill(0, 64, 96);
  set32(blank, 52, 1);
  const empty = await openParallels(memoryFile(blank).file);
  expect((await empty.read(0, MiB)).some(Boolean)).toBe(false);
  const partial = source.slice();
  set64(partial, 36, (7 * MiB + 512) / 512);
  const tail = await openParallels(memoryFile(partial).file);
  expect(
    (await tail.read(7 * MiB, 512)).every(
      (v, i) => v === expected[7 * MiB + i],
    ),
  ).toBe(true);
  await expect(tail.read(7 * MiB + 511, 2)).rejects.toThrow("越界");
  // A synthetic valid non-power-of-two cluster layout, independent of the reader.
  const cluster = 3 * 512,
    size = 4 * MiB,
    entries = Math.ceil(size / cluster);
  const data = Math.ceil((64 + entries * 4) / cluster) * cluster;
  const b = new Uint8Array(data + cluster);
  b.set(source.subarray(0, 64));
  set32(b, 28, 3);
  set32(b, 32, entries);
  set64(b, 36, size / 512);
  set32(b, 48, data / 512);
  set32(b, 64, data / cluster);
  b.fill(0x5a, data);
  const image = await openParallels(memoryFile(b).file),
    bytes = await image.read(cluster - 1, 514);
  expect(bytes[0]).toBe(0x5a);
  expect(bytes.subarray(1).some(Boolean)).toBe(false);
});
it("supports legacy explicit/implicit data offsets, closed 2.1 status and zero extra map entries", async () => {
  const old = fixture("legacy");
  set32(old, 48, 1);
  set32(old, 44, 0x312e3276);
  expect((await openParallels(memoryFile(old).file)).size).toBe(8 * MiB);
  const extra = fixture();
  set32(extra, 32, 12);
  const image = await openParallels(memoryFile(extra).file),
    expected = raw();
  expect((await image.read(0, 512)).every((v, i) => v === expected[i])).toBe(
    true,
  );
  set32(extra, 64 + 8 * 4, 1);
  await expect(openParallels(memoryFile(extra).file)).rejects.toThrow(
    "容量之外",
  );
});
it("rejects unknown/dirty/converting headers, format extensions and implausible geometry before mapping reads", async () => {
  for (const [at, value] of [
    [0, 0],
    [16, 1],
    [16, 3],
    [28, 0],
    [28, 0xffffffff],
    [32, 0],
    [32, 7],
    [32, 0xffffffff],
    [44, 0x746f6e59],
    [44, 1],
    [48, 0],
    [48, 1],
    [48, 2049],
    [48, 0xffffffff],
    [52, 2],
    [52, 4],
    [56, 1],
  ]) {
    const b = fixture();
    set32(b, at, value);
    const input = memoryFile(b);
    await expect(openParallels(input.file)).rejects.toThrow();
    expect(input.reads).toEqual([[0, 64]]);
  }
  for (const sectors of [0n, 1n, 8191n, 2n ** 34n + 1n, 2n ** 63n]) {
    const b = fixture();
    set64(b, 36, sectors);
    await expect(openParallels(memoryFile(b).file)).rejects.toThrow("逻辑容量");
  }
  const legacy = fixture("legacy");
  set32(legacy, 40, 1);
  await expect(openParallels(memoryFile(legacy).file)).rejects.toThrow(
    "保留容量",
  );
});
it("rejects data aliases, header overlap, bad legacy alignment and incomplete physical tails", async () => {
  for (const [index, value] of [
    [0, 0xffffffff],
    [0, 5],
    [0, get32(fixture(), 68)],
    [1, 1],
  ]) {
    const b = fixture();
    set32(b, 64 + index * 4, value);
    await expect(openParallels(memoryFile(b).file)).rejects.toThrow();
  }
  const old = fixture("legacy");
  set32(old, 64, 2);
  await expect(openParallels(memoryFile(old).file)).rejects.toThrow("未对齐");
  const small = fixture("small");
  set32(small, 64, 1);
  // This duplicates the next allocated cluster when present, otherwise use exact second slot.
  set32(small, 68, 1);
  await expect(openParallels(memoryFile(small).file)).rejects.toThrow("重复");
  const emptyFlag = fixture();
  set32(emptyFlag, 52, 1);
  await expect(openParallels(memoryFile(emptyFlag).file)).rejects.toThrow(
    "空盘标志",
  );
  for (const cut of [1, 512, MiB])
    await expect(
      openParallels(memoryFile(fixture().subarray(0, 5 * MiB - cut)).file),
    ).rejects.toThrow();
});
it("does not read unreferenced blocks and reports that they were ignored", async () => {
  const source = fixture(),
    b = new Uint8Array(source.length + MiB);
  b.set(source);
  b.fill(0xa9, source.length);
  const input = memoryFile(b),
    image = await openParallels(input.file);
  expect(image.warnings.some((n) => n.includes("未被索引引用"))).toBe(true);
  await image.read(7 * MiB, MiB);
  await image.verify();
  expect(input.reads.every(([, z]) => z <= source.length)).toBe(true);
});
it("rechecks all header and table bytes before accepting a logical view", async () => {
  for (const at of [0, 16, 20, 24, 28, 32, 36, 44, 48, 52, 56, 64, 80, 92]) {
    const b = fixture(),
      image = await openParallels(memoryFile(b).file);
    b[at] ^= 1;
    await expect(image.verify()).rejects.toThrow("发生变化");
  }
});
it("rejects invalid reads and truncated physical slices and enforces work budgets", async () => {
  const b = fixture(),
    image = await openParallels(memoryFile(b).file);
  for (const [at, length] of [
    [-1, 512],
    [NaN, 512],
    [0.5, 512],
    [0, 0],
    [0, 0.5],
    [0, NaN],
    [8 * MiB, 1],
  ])
    await expect(image.read(at, length)).rejects.toThrow();
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  await expect(
    (await openParallels(memoryFile(b).file)).read(0, MiB + 1),
  ).rejects.toThrow("有限字节");
  for (const shortAt of [0, 64, get32(b, 64) * MiB]) {
    const f = memoryFile(b).file,
      slice = f.slice.bind(f);
    f.slice = ((a: number, z: number) =>
      slice(a, a === shortAt ? z - 1 : z)) as File["slice"];
    await expect(
      (async () => {
        const image = await openParallels(f);
        await image.read(0, 512);
      })(),
    ).rejects.toThrow("不完整");
  }
});
it("supports 8 TiB sparse geometry with a bounded BAT and caps cumulative physical reads", async () => {
  const head = fixture().slice(0, 64),
    mapLength = 32 * MiB,
    dataStart = 33 * MiB;
  set64(head, 36, 2n ** 34n);
  set32(head, 32, mapLength / 4);
  set32(head, 48, dataStart / 512);
  let read = 0;
  const file = {
    size: dataStart,
    slice(a: number, z: number) {
      read += z - a;
      return {
        arrayBuffer: async () =>
          a === 0 ? head.slice().buffer : new Uint8Array(z - a).buffer,
      };
    },
  } as unknown as File;
  const image = await openParallels(file);
  expect((await image.read(2 ** 43 - MiB, MiB)).some(Boolean)).toBe(false);
  await image.verify();
  expect(read).toBe(2 * (64 + mapLength));
  await expect(image.verify()).rejects.toThrow("资源上限");
  set32(head, 32, mapLength / 4 + 1);
  await expect(openParallels(file)).rejects.toThrow("32 MiB");
});

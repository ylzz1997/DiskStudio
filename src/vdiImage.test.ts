import { expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { openVdi } from "./vdiImage";
import { logicalDisk, memoryFile, MiB } from "../tests/fixtures/vhd-memory";
import { vdiBytes, le32, le64, get32 } from "../tests/fixtures/vdi-memory";

it("decodes independent QEMU fixed and dynamic containers into exact raw/EBR bytes", async () => {
  const raw = gunzipSync(
    readFileSync(new URL("../tests/fixtures/vhd-qemu-raw.gz", import.meta.url)),
  );
  for (const kind of ["fixed", "dynamic"]) {
    const b = gunzipSync(
      readFileSync(
        new URL(`../tests/fixtures/vdi-qemu-${kind}.gz`, import.meta.url),
      ),
    );
    const before = Uint8Array.from(b),
      input = memoryFile(b, "independent.vdi"),
      image = await openVdi(input.file);
    expect(image.format).toBe(
      kind === "fixed" ? "VDI · 固定容量" : "VDI · 动态扩展",
    );
    expect(image.size).toBe(raw.length);
    expect(image.sectorSize).toBe(512);
    expect(image.warnings).toEqual([]);
    for (const [at, length] of [
      [0, MiB],
      [7 * MiB, MiB],
      [MiB, 512],
      [4 * MiB, 512],
      [MiB - 511, 1025],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === raw[at + i]),
      ).toBe(true);
    await image.verify();
    expect(b.every((v, i) => v === before[i])).toBe(true);
    expect(input.reads.reduce((sum, [a, z]) => sum + z - a, 0)).toBeLessThan(
      3 * MiB,
    );
  }
});
it("uses mapped physical order, skips per-block extras and bounds a partial logical tail", async () => {
  const raw = logicalDisk().subarray(0, 7 * MiB + 512);
  for (const options of [
    { fixed: true },
    { plus: true, extra: 512 },
    { block: 4096, extra: 512 },
    { block: 4 * MiB, extra: MiB },
  ]) {
    const b = vdiBytes(raw, options),
      original = b.slice(),
      image = await openVdi(memoryFile(b).file);
    for (const [at, length] of [
      [0, MiB],
      [MiB - 29, 1024],
      [raw.length - MiB, MiB],
      [4095, 514],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === raw[at + i]),
      ).toBe(true);
    await image.verify();
    expect(b.every((v, i) => v === original[i])).toBe(true);
  }
});
it("zero-fills only free/zero map entries and never leaks extra bytes or unused tail contents", async () => {
  const raw = logicalDisk(),
    b = vdiBytes(raw, { sparse: true, extra: 512 });
  const tail = new Uint8Array(b.length + MiB);
  tail.set(b);
  tail.fill(0xa9, b.length);
  const input = memoryFile(tail),
    image = await openVdi(input.file);
  const got = await image.read(MiB - 256, MiB);
  expect(got.subarray(0, 256)).toEqual(raw.subarray(MiB - 256, MiB));
  expect(got.subarray(256).some(Boolean)).toBe(false);
  expect((await image.read(2 * MiB, MiB)).some(Boolean)).toBe(false);
  expect(image.notes.filter((n) => n.includes("未分配")).length).toBe(1);
  expect(image.warnings[0]).toContain("额外字节");
  expect(input.reads.every(([a]) => a < b.length)).toBe(true);
  const blank = vdiBytes(new Uint8Array(4 * MiB), { sparse: true });
  le32(blank, 388, 0);
  blank.fill(0xff, 512, 528);
  const empty = await openVdi(
    memoryFile(blank.subarray(0, get32(blank, 344))).file,
  );
  expect((await empty.read(0, MiB)).some(Boolean)).toBe(false);
});
it("rejects unsupported identities, versions, parent dependencies and flags before block-map reads", async () => {
  const cases: [number, number][] = [
    [64, 0],
    [68, 2],
    [68, 0x10000],
    [68, 0x20001],
    [72, 383],
    [72, 392],
    [72, 65536],
    [76, 0],
    [76, 3],
    [76, 4],
    [76, 5],
    [80, 2],
    [80, 0x80000000],
    [360, 4096],
    [424, 1],
    [440, 1],
  ];
  for (const [at, value] of cases) {
    const b = vdiBytes();
    le32(b, at, value);
    const input = memoryFile(b);
    await expect(openVdi(input.file)).rejects.toThrow();
    expect(input.reads.every(([start]) => start < 512)).toBe(true);
  }
  for (const at of [392, 408]) {
    const b = vdiBytes();
    b.fill(0, at, at + 16);
    await expect(openVdi(memoryFile(b).file)).rejects.toThrow("标识为空");
  }
  const plus = vdiBytes(undefined, { plus: true });
  le32(plus, 468, 4096);
  await expect(openVdi(memoryFile(plus).file)).rejects.toThrow("512 B");
  const zeroExpand = vdiBytes();
  le32(zeroExpand, 80, 1);
  expect((await openVdi(memoryFile(zeroExpand).file)).format).toContain("VDI");
});
it("rejects invalid geometry and overlapping/truncated metadata before reading data", async () => {
  for (const [at, value] of [
    [340, 0],
    [340, 456],
    [340, 513],
    [344, 512],
    [344, 1025],
    [344, 0xfffffe00],
    [376, 0],
    [376, 256],
    [376, 3 * MiB],
    [376, 512 * MiB],
    [380, 1],
    [380, 768],
    [380, 2 * MiB],
    [384, 7],
    [384, 9],
    [388, 9],
  ]) {
    const b = vdiBytes();
    le32(b, at, value);
    const input = memoryFile(b);
    await expect(openVdi(input.file)).rejects.toThrow();
    expect(input.reads.every(([start]) => start < 512)).toBe(true);
  }
  for (const size of [
    0n,
    4n * BigInt(MiB) - 512n,
    BigInt(8 * MiB + 1),
    2n ** 43n + 512n,
    2n ** 63n,
  ]) {
    const b = vdiBytes();
    le64(b, 368, size);
    await expect(openVdi(memoryFile(b).file)).rejects.toThrow("逻辑容量");
  }
  const truncated = vdiBytes().subarray(0, 8 * MiB);
  await expect(openVdi(memoryFile(truncated).file)).rejects.toThrow(
    "数据区不完整",
  );
  const fixedSparse = vdiBytes(undefined, { fixed: true, sparse: true });
  await expect(openVdi(memoryFile(fixedSparse).file)).rejects.toThrow(
    "固定 VDI",
  );
});
it("rejects duplicate, out-of-range and orphaned allocated slots without treating them as raw data", async () => {
  for (const value of [
    get32(vdiBytes(), 516),
    8,
    0xfffffffd,
    0xfffffffe,
    0xffffffff,
  ]) {
    const b = vdiBytes();
    le32(b, 512, value);
    const input = memoryFile(b);
    await expect(openVdi(input.file)).rejects.toThrow(/映射|逻辑块|分配块数/);
    expect(input.reads.every(([a]) => a < get32(b, 344))).toBe(true);
  }
});
it("rereads preheader, identities, extended geometry and the entire map before accepting", async () => {
  for (const at of [
    0, 64, 68, 72, 84, 340, 368, 392, 408, 456, 468, 512, 540,
  ]) {
    const b = vdiBytes(undefined, { plus: true }),
      image = await openVdi(memoryFile(b).file);
    b[at] ^= 1;
    await expect(image.verify()).rejects.toThrow("发生变化");
  }
});
it("bounds offsets, single/cumulative logical reads and detects short reads", async () => {
  const b = vdiBytes(),
    image = await openVdi(memoryFile(b).file);
  for (const [at, n] of [
    [-1, 512],
    [NaN, 512],
    [0.5, 512],
    [0, 0],
    [0, NaN],
    [0, 0.5],
    [8 * MiB, 1],
    [7 * MiB, MiB + 1],
  ])
    await expect(image.read(at, n)).rejects.toThrow();
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  const tooBig = await openVdi(memoryFile(b).file);
  await expect(tooBig.read(0, MiB + 1)).rejects.toThrow("有限字节");
  for (const truncateAt of [0, 76, 512, get32(b, 344)]) {
    const f = memoryFile(b).file,
      slice = f.slice.bind(f);
    f.slice = ((start: number, end: number) =>
      slice(start, start === truncateAt ? end - 1 : end)) as File["slice"];
    const action = async () => {
      const i = await openVdi(f);
      await i.read(7 * MiB, 512);
    };
    await expect(action()).rejects.toThrow("读取不完整");
  }
});
it("accepts an 8 TiB sparse logical disk with bounded metadata, and enforces the file-read budget", async () => {
  const head = vdiBytes().slice(0, 512),
    mapLength = 32 * MiB;
  le64(head, 368, 2n ** 43n);
  le32(head, 384, mapLength / 4);
  le32(head, 388, 0);
  le32(head, 344, 512 + mapLength);
  let bytesRead = 0;
  const file = {
    size: 512 + mapLength,
    slice(a: number, z: number) {
      bytesRead += z - a;
      return {
        arrayBuffer: async () =>
          a < 512
            ? head.slice(a, z).buffer
            : new Uint8Array(z - a).fill(0xff).buffer,
      };
    },
  } as unknown as File;
  const image = await openVdi(file);
  expect((await image.read(2 ** 43 - MiB, MiB)).some(Boolean)).toBe(false);
  await image.verify();
  expect(bytesRead).toBeLessThan(65 * MiB);
  await expect(image.verify()).rejects.toThrow("资源上限");
  le32(head, 376, MiB / 2);
  le32(head, 384, mapLength / 2);
  await expect(openVdi(file)).rejects.toThrow("32 MiB");
});

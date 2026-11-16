import { expect, it } from "vitest";
import { crc32c, openVhdx } from "./vhdxImage";
import { memoryFile } from "../tests/fixtures/vhd-memory";
import {
  fixture,
  rawFixture,
  MiB,
  K,
  BAT,
  META,
  le32,
  le64,
  get64,
  item,
  seal,
} from "../tests/fixtures/vhdx-data";
it("uses CRC-32C and reads independent fixed, dynamic and 4Kn fixture bytes exactly", async () => {
  expect(crc32c(new TextEncoder().encode("123456789"))).toBe(0xe3069283);
  for (const kind of ["fixed", "dynamic", "gpt4k"]) {
    const bytes = fixture(kind),
      before = bytes.slice(),
      source = memoryFile(bytes),
      image = await openVhdx(source.file),
      raw = rawFixture(kind === "gpt4k");
    expect(image.sectorSize).toBe(kind === "gpt4k" ? 4096 : 512);
    expect(image.format).toBe(
      kind === "fixed" ? "VHDX · 固定容量" : "VHDX · 动态扩展",
    );
    expect(image.warnings).toEqual([]);
    expect(image.size).toBe(raw.length);
    for (const [at, len] of [
      [0, MiB],
      [raw.length - MiB, MiB],
      [MiB, 512],
      [4 * MiB, 512],
      [MiB - 129, 1025],
    ])
      expect(
        (await image.read(at, len)).every((b, i) => b === raw[at + i]),
      ).toBe(true);
    await image.verify();
    expect(bytes.every((b, i) => b === before[i])).toBe(true);
    expect(source.reads.reduce((n, [a, b]) => n + b - a, 0)).toBeLessThan(
      3 * MiB,
    );
  }
  const mixed = fixture("gpt4k");
  le32(mixed, item(mixed, 4), 512);
  expect((await openVhdx(memoryFile(mixed).file)).sectorSize).toBe(4096);
});
it("selects only the highest valid 64-bit header and exposes single-copy failures", async () => {
  const good = fixture();
  for (const offset of [64 * K, 128 * K]) {
    const b = good.slice();
    b[offset] ^= 1;
    const image = await openVhdx(memoryFile(b).file);
    expect(image.warnings[0]).toContain("只有一份");
  }
  const both = good.slice();
  both[64 * K] ^= 1;
  both[128 * K] ^= 1;
  await expect(openVhdx(memoryFile(both).file)).rejects.toThrow("两份头部");
  const equal = good.slice();
  le64(equal, 64 * K + 8, get64(equal, 128 * K + 8));
  seal(equal, 64 * K, 4096);
  await expect(openVhdx(memoryFile(equal).file)).rejects.toThrow("序号相同");
  const oldDirty = good.slice();
  le64(oldDirty, 64 * K + 8, 2n ** 63n);
  le64(oldDirty, 128 * K + 8, 2n ** 63n + 1n);
  oldDirty[64 * K + 48] = 1;
  seal(oldDirty, 64 * K, 4096);
  seal(oldDirty, 128 * K, 4096);
  expect((await openVhdx(memoryFile(oldDirty).file)).warnings).toEqual([]);
  oldDirty[128 * K + 48] = 1;
  seal(oldDirty, 128 * K, 4096);
  const input = memoryFile(oldDirty);
  await expect(openVhdx(input.file)).rejects.toThrow("日志");
  expect(input.reads).toHaveLength(3);
  oldDirty[128 * K + 48] = 0;
  oldDirty[128 * K + 66] = 2;
  seal(oldDirty, 128 * K, 4096);
  await expect(openVhdx(memoryFile(oldDirty).file)).rejects.toThrow(
    "当前头部版本",
  );
});
it("validates both region tables, permits entry ordering differences and rejects conflicts", async () => {
  const good = fixture();
  for (const offset of [192 * K, 256 * K]) {
    const b = good.slice();
    b[offset] ^= 1;
    expect((await openVhdx(memoryFile(b).file)).warnings[0]).toContain(
      "区域表",
    );
  }
  const reorder = good.slice(),
    first = reorder.slice(256 * K + 16, 256 * K + 48);
  reorder.copyWithin(256 * K + 16, 256 * K + 48, 256 * K + 80);
  reorder.set(first, 256 * K + 48);
  seal(reorder, 256 * K, 64 * K);
  await expect(openVhdx(memoryFile(reorder).file)).resolves.toMatchObject({
    size: 8 * MiB,
  });
  const conflict = good.slice();
  le64(conflict, 256 * K + 16 + 16, 4 * MiB);
  seal(conflict, 256 * K, 64 * K);
  await expect(openVhdx(memoryFile(conflict).file)).rejects.toThrow("冲突");
  for (const mutate of [
    (b: Uint8Array) => le32(b, 192 * K + 8, 2048),
    (b: Uint8Array) => le64(b, 192 * K + 32, MiB),
    (b: Uint8Array) => le64(b, 192 * K + 32, 2 * MiB + 1),
    (b: Uint8Array) => le64(b, 192 * K + 32, 2n ** 63n),
    (b: Uint8Array) => {
      b[192 * K + 16] ^= 1;
    },
    (b: Uint8Array) => le32(b, 192 * K + 44, 2),
    (b: Uint8Array) => {
      b.copyWithin(192 * K + 48, 192 * K + 16, 192 * K + 32);
    },
  ]) {
    const b = good.slice();
    mutate(b);
    seal(b, 192 * K, 64 * K);
    b.set(b.subarray(192 * K, 256 * K), 256 * K);
    await expect(openVhdx(memoryFile(b).file)).rejects.toThrow();
  }
});
it("rejects malformed, duplicate, overlapping, unknown-required and parent metadata", async () => {
  const good = fixture();
  for (const mutate of [
    (b: Uint8Array) => {
      b[META] ^= 1;
    },
    (b: Uint8Array) => {
      b[META + 10] = 0xff;
      b[META + 11] = 0xff;
    },
    (b: Uint8Array) => le32(b, META + 32 + 16, 63 * K),
    (b: Uint8Array) => le32(b, META + 32 + 20, MiB + 1),
    (b: Uint8Array) => le32(b, META + 32 + 24, 0),
    (b: Uint8Array) => le32(b, META + 32 + 28, 1),
    (b: Uint8Array) => {
      b.copyWithin(META + 64, META + 32, META + 48);
    },
    (b: Uint8Array) => le32(b, META + 64 + 16, item(b, 0) - META),
    (b: Uint8Array) => {
      b[META + 32] ^= 1;
    },
    (b: Uint8Array) => le32(b, item(b, 0) + 4, 2),
    (b: Uint8Array) => le32(b, item(b, 0), 3 * MiB),
    (b: Uint8Array) => le64(b, item(b, 1), 2n ** 63n),
    (b: Uint8Array) => le32(b, item(b, 3), 2048),
    (b: Uint8Array) => le32(b, item(b, 4), 256),
  ]) {
    const b = good.slice();
    mutate(b);
    await expect(openVhdx(memoryFile(b).file)).rejects.toThrow();
  }
  const optional = good.slice();
  optional[META + 10] = 6;
  const p = META + 32 + 5 * 32;
  optional.fill(0x9a, p, p + 16);
  le32(optional, p + 24, 1);
  await expect(openVhdx(memoryFile(optional).file)).resolves.toMatchObject({
    size: 8 * MiB,
  });
  le32(optional, p + 24, 5);
  await expect(openVhdx(memoryFile(optional).file)).rejects.toThrow(
    "必需元数据",
  );
});
it("maps all standalone zero states without exposing stale stored data", async () => {
  for (const state of [0, 1, 2, 3]) {
    const b = fixture(),
      old = get64(b, BAT);
    le64(
      b,
      BAT,
      state === 1 || state === 3 ? (old & ~7n) | BigInt(state) : state,
    );
    const image = await openVhdx(memoryFile(b).file);
    expect((await image.read(0, 512)).some(Boolean)).toBe(false);
    expect(image.notes.length).toBe(state === 2 ? 0 : 1);
  }
});
it("rejects reserved BAT bits/states, allocated bitmap blocks, overlap and out-of-file locations", async () => {
  const good = fixture();
  for (const value of [
    4n,
    5n,
    7n,
    6n,
    get64(good, BAT) | 8n,
    BigInt(BAT) | 6n,
    get64(good, BAT + 8),
    BigInt(good.length) | 6n,
    (2n ** 63n) | 6n,
    BigInt(8 * MiB) | 2n,
  ]) {
    const b = good.slice();
    le64(b, BAT, value);
    await expect(openVhdx(memoryFile(b).file)).rejects.toThrow();
  }
});
it("handles chunk-boundary bitmap slots and exact final chunks without allocating logical capacity", async () => {
  for (const blocks of [4096, 4097, 8192, 8193]) {
    const b = fixture(),
      payload = get64(b, BAT);
    le64(b, item(b, 1), blocks * MiB);
    b.fill(0, BAT, BAT + MiB);
    const index = blocks - 1 + Math.floor((blocks - 1) / 4096);
    le64(b, BAT + index * 8, payload);
    const image = await openVhdx(memoryFile(b).file);
    expect(await image.read((blocks - 1) * MiB, 512)).toEqual(
      rawFixture().subarray(0, 512),
    );
    expect(
      (await image.read((blocks - 1) * MiB - 512, 512)).some(Boolean),
    ).toBe(false);
    if (blocks > 4096) {
      le64(b, BAT + 4096 * 8, payload);
      await expect(openVhdx(memoryFile(b).file)).rejects.toThrow("位图");
    }
  }
});
it("bounds declared metadata/BAT size, virtual ranges, short reads and cumulative work", async () => {
  const tooBig = fixture();
  le64(tooBig, item(tooBig, 1), 8 * 1024 ** 4);
  await expect(openVhdx(memoryFile(tooBig).file)).rejects.toThrow("BAT");
  for (const [at, len] of [
    [-1, 512],
    [0, MiB + 1],
    [8 * MiB, 1],
    [NaN, 1],
    [0, 0],
  ]) {
    const image = await openVhdx(memoryFile(fixture()).file);
    await expect(image.read(at, len)).rejects.toThrow();
  }
  const image = await openVhdx(memoryFile(fixture()).file);
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  const short = memoryFile(fixture()).file;
  short.slice = (() => ({
    arrayBuffer: async () => new ArrayBuffer(0),
  })) as File["slice"];
  await expect(openVhdx(short)).rejects.toThrow("读取不完整");
});
it("rechecks headers, region maps, metadata and BAT before accepting a read", async () => {
  for (const offset of [
    0,
    64 * K,
    128 * K,
    192 * K,
    256 * K,
    META,
    META + 65536,
    BAT,
  ]) {
    const b = fixture(),
      image = await openVhdx(memoryFile(b).file);
    await image.read(0, 512);
    b[offset] ^= 1;
    await expect(image.verify()).rejects.toThrow("发生变化");
  }
});
it("reserves unknown optional regions while refusing required extensions and log overlap", async () => {
  const b = fixture(),
    p = 192 * K + 16 + 2 * 32;
  le32(b, 192 * K + 8, 3);
  b.fill(0x99, p, p + 16);
  le64(b, p + 16, 4 * MiB);
  le32(b, p + 24, MiB);
  le32(b, p + 28, 0);
  const regions = () => {
    seal(b, 192 * K, 64 * K);
    b.set(b.subarray(192 * K, 256 * K), 256 * K);
  };
  regions();
  await expect(openVhdx(memoryFile(b).file)).resolves.toMatchObject({
    size: 8 * MiB,
  });
  le32(b, p + 28, 1);
  regions();
  await expect(openVhdx(memoryFile(b).file)).rejects.toThrow("必需区域");
  le32(b, p + 28, 0);
  regions();
  le64(b, BAT, BigInt(4 * MiB) | 6n);
  await expect(openVhdx(memoryFile(b).file)).rejects.toThrow("元数据重叠");
  const c = fixture();
  for (const at of [64 * K, 128 * K]) {
    le64(c, at + 72, 2 * MiB);
    seal(c, at, 4096);
  }
  await expect(openVhdx(memoryFile(c).file)).rejects.toThrow("容器区域重叠");
});

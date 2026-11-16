import { expect, it, vi } from "vitest";
import { openVmdk } from "./vmdkImage";
import { memoryFile, MiB } from "../tests/fixtures/vhd-memory";
import {
  fixture,
  raw,
  inflate,
  table,
  entry,
  u32,
  u64,
  set32,
  set64,
  editDescriptor,
} from "../tests/fixtures/vmdk-data";

it("decodes independent QEMU sparse, zero-enabled and compressed layouts including footer indexes", async () => {
  const expected = raw();
  for (const kind of ["sparse", "zero", "stream", "footer"]) {
    const b = fixture(kind),
      before = b.slice(),
      input = memoryFile(b),
      decoder = vi.fn(inflate);
    const image = await openVmdk(input.file, decoder);
    expect(image.size).toBe(expected.length);
    expect(image.sectorSize).toBe(512);
    expect(image.format).toContain(
      kind === "stream" || kind === "footer" ? "压缩流" : "单文件稀疏",
    );
    for (const [at, length] of [
      [0, MiB],
      [7 * MiB, MiB],
      [MiB, 512],
      [4 * MiB, 512],
      [65536 - 31, 513],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === expected[at + i]),
      ).toBe(true);
    await image.verify();
    expect(b.every((v, i) => v === before[i])).toBe(true);
    expect(input.reads.reduce((n, [a, z]) => n + z - a, 0)).toBeLessThan(
      2 * MiB,
    );
    expect(decoder.mock.calls.length > 0).toBe(
      kind === "stream" || kind === "footer",
    );
  }
});
it("uses grain mapping rather than physical order, crosses tables and handles sparse/explicit-zero grains", async () => {
  const b = fixture("zero"),
    gt = table(b),
    first = u32(b, gt),
    last = u32(b, gt + 127 * 4);
  const tmp = b.slice(first * 512, first * 512 + 65536);
  b.copyWithin(first * 512, last * 512, last * 512 + 65536);
  b.set(tmp, last * 512);
  entry(b, 0, last);
  entry(b, 127, first);
  entry(b, 3, 1);
  const image = await openVmdk(memoryFile(b).file),
    expected = raw();
  expect((await image.read(0, MiB)).every((v, i) => v === expected[i])).toBe(
    true,
  );
  expect(
    (await image.read(7 * MiB, MiB)).every(
      (v, i) => v === expected[7 * MiB + i],
    ),
  ).toBe(true);
  // A separate second directory points to the old single GT. The first is sparse.
  const multi = fixture(),
    gd = u64(multi, 56) * 512,
    rgd = u64(multi, 48) * 512;
  for (const at of [gd, rgd]) {
    set32(multi, at + 4, u32(multi, at));
    set32(multi, at, 0);
  }
  set64(multi, 12, (40 * MiB) / 512);
  editDescriptor(multi, (t) => t.replace("RW 16384", `RW ${(40 * MiB) / 512}`));
  const moved = await openVmdk(memoryFile(multi).file);
  const boundary = await moved.read(32 * MiB - 512, 1024);
  expect(boundary.subarray(0, 512).some(Boolean)).toBe(false);
  expect(boundary.subarray(512).every((v, i) => v === expected[i])).toBe(true);
});
it("rejects unsupported headers, dirty state, invalid geometry and unsafe integer fields", async () => {
  for (const [at, value] of [
    [0, 0],
    [4, 4],
    [8, 8],
    [44, 511],
    [72, 1],
    [73, 0],
    [77, 1],
  ]) {
    const b = fixture();
    set32(b, at, value);
    await expect(openVmdk(memoryFile(b).file)).rejects.toThrow();
  }
  for (const [at, value] of [
    [12, 0n],
    [12, 2n ** 63n],
    [12, 2n ** 35n],
    [20, 0n],
    [20, 3n],
    [20, 24n],
    [28, 0n],
    [28, 2n ** 63n],
    [36, 0n],
    [36, 129n],
    [48, 0n],
    [56, 0n],
    [64, 0n],
    [64, 999999n],
  ] as [number, bigint][]) {
    const b = fixture();
    set64(b, at, value);
    await expect(openVmdk(memoryFile(b).file)).rejects.toThrow();
  }
  const tooMany = fixture();
  set64(tooMany, 12, 2 ** 43 / 512);
  set64(tooMany, 20, 16);
  editDescriptor(tooMany, (t) => t.replace("RW 16384", `RW ${2 ** 43 / 512}`));
  await expect(openVmdk(memoryFile(tooMany).file)).rejects.toThrow("32 MiB");
});
it("rejects parent chains, external paths, other extent types, duplicate fields and incompatible descriptors before data reads", async () => {
  const edits = [
    (s: string) => s.replace("parentCID=ffffffff", "parentCID=12345678"),
    (s: string) => s + '\nparentFileNameHint="parent.vmdk"',
    (s: string) => s.replace('"synthetic-sparse.vmdk"', '"C:\\disk.vmdk"'),
    (s: string) => s.replace('"synthetic-sparse.vmdk"', '"../disk.vmdk"'),
    (s: string) => s.replace(" SPARSE ", " FLAT "),
    (s: string) => s.replace("RW 16384", "RW 12345"),
    (s: string) => s + '\nRW 16384 SPARSE "second.vmdk"',
    (s: string) => s + "\nVersion=1",
    (s: string) => s + '\nddb.encryptionKey="secret"',
    (s: string) => s + '\nddb.logicalSectorSize="4096"',
    (s: string) => s + '\nunknownFeature="1"',
    (s: string) => s.replace("monolithicSparse", "twoGbMaxExtentSparse"),
    (s: string) => s + '\nencoding="utf-16"',
  ];
  for (const edit of edits) {
    const b = fixture();
    editDescriptor(b, edit);
    const input = memoryFile(b);
    await expect(openVmdk(input.file)).rejects.toThrow();
    expect(input.reads.every(([a]) => a < 21 * 512)).toBe(true);
  }
  const caseFold = fixture();
  editDescriptor(caseFold, (s) =>
    s.replace("version=1", "VERSION = 1").replace("RW ", "rdonly "),
  );
  expect((await openVmdk(memoryFile(caseFold).file)).size).toBe(8 * MiB);
  for (const at of [512, 10 * 512]) {
    const bad = fixture();
    bad[at] = 0xff;
    await expect(openVmdk(memoryFile(bad).file)).rejects.toThrow(/编码|终止符/);
  }
});
it("rejects conflicting redundant maps, metadata aliases, invalid data grains and reserved entries", async () => {
  const alterations = [
    (b: Uint8Array) => set32(b, table(b, true), 0),
    (b: Uint8Array) => set32(b, u64(b, 48) * 512, 0),
    (b: Uint8Array) => set32(b, u64(b, 48) * 512, table(b) / 512),
    (b: Uint8Array) => entry(b, 0, 1),
    (b: Uint8Array) => entry(b, 0, 0xffffffff),
    (b: Uint8Array) => entry(b, 0, 21),
    (b: Uint8Array) => entry(b, 0, 129),
    (b: Uint8Array) => entry(b, 1, u32(b, table(b))),
    (b: Uint8Array) => entry(b, 128, 128),
  ];
  for (const alter of alterations) {
    const b = fixture();
    alter(b);
    await expect(openVmdk(memoryFile(b).file)).rejects.toThrow();
  }
  const b = fixture();
  await expect(
    openVmdk(memoryFile(b.subarray(0, b.length - 65536)).file),
  ).rejects.toThrow();
});
it("validates footer/EOS and metadata markers and prevents compressed grains from overlapping tables", async () => {
  for (const mutate of [
    (b: Uint8Array) => (b[b.length - 1536 + 12] ^= 1),
    (b: Uint8Array) => (b[b.length - 512] = 1),
    (b: Uint8Array) => (b[b.length - 1024 + 12] ^= 1),
    (b: Uint8Array) => (b[table(b) - 512 + 12] ^= 1),
    (b: Uint8Array) => entry(b, 0, table(b) / 512),
  ]) {
    const b = fixture("footer");
    mutate(b);
    await expect(openVmdk(memoryFile(b).file, inflate)).rejects.toThrow();
  }
  const b = fixture("footer"),
    at = u32(b, table(b) + 127 * 4) * 512;
  set32(b, at + 8, 2 * 65536);
  const image = await openVmdk(memoryFile(b).file, inflate);
  await expect(image.read(127 * 65536, 512)).rejects.toThrow(/越界|重叠/);
});
it("checks compressed LBA, length, payload bounds, checksum and actual decoded size", async () => {
  for (const mutate of [
    (b: Uint8Array, at: number) => set64(b, at, 128),
    (b: Uint8Array, at: number) => set32(b, at + 8, 0),
    (b: Uint8Array, at: number) => set32(b, at + 8, 2 * 65536 + 1),
    (b: Uint8Array, at: number) => set32(b, at + 8, 2 * 65536),
    (b: Uint8Array, at: number) => (b[at + 11 + u32(b, at + 8)] ^= 1),
  ]) {
    const b = fixture("stream"),
      at = u32(b, table(b)) * 512;
    mutate(b, at);
    const image = await openVmdk(memoryFile(b).file, inflate);
    await expect(image.read(0, 512)).rejects.toThrow();
  }
  const b = fixture("stream"),
    input = memoryFile(b);
  await expect((await openVmdk(input.file)).read(0, 512)).rejects.toThrow(
    "桌面程序",
  );
  await expect(
    (await openVmdk(input.file, async () => new Uint8Array(3))).read(0, 512),
  ).rejects.toThrow("长度");
});
it("caches decoded grains and rereads all metadata and used compressed markers on acceptance", async () => {
  const b = fixture("stream"),
    decoder = vi.fn(inflate),
    image = await openVmdk(memoryFile(b).file, decoder);
  await image.read(0, 512);
  await image.read(256, 512);
  expect(decoder).toHaveBeenCalledTimes(1);
  b[u32(b, table(b)) * 512 + 8] ^= 1;
  await expect(image.verify()).rejects.toThrow("发生变化");
  for (const kind of ["sparse", "footer"]) {
    const base = fixture(kind);
    for (const at of [
      0,
      512,
      table(base),
      kind === "footer" ? base.length - 1024 : table(base, true),
    ]) {
      const changed = base.slice(),
        image = await openVmdk(memoryFile(changed).file, inflate);
      changed[at] ^= 1;
      await expect(image.verify()).rejects.toThrow("发生变化");
    }
  }
});
it("bounds logical reads and detects truncated physical reads", async () => {
  const b = fixture(),
    image = await openVmdk(memoryFile(b).file);
  for (const [at, length] of [
    [-1, 512],
    [NaN, 512],
    [0.5, 512],
    [0, 0],
    [0, NaN],
    [0, 0.5],
    [8 * MiB, 1],
  ])
    await expect(image.read(at, length)).rejects.toThrow();
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  await expect(
    (await openVmdk(memoryFile(b).file)).read(0, MiB + 1),
  ).rejects.toThrow("有限字节");
  for (const shortAt of [0, 512, u64(b, 56) * 512, table(b), 65536]) {
    const file = memoryFile(b).file,
      slice = file.slice.bind(file);
    file.slice = ((a: number, z: number) =>
      slice(a, a === shortAt ? z - 1 : z)) as File["slice"];
    await expect(
      (async () => {
        const image = await openVmdk(file);
        await image.read(0, 512);
      })(),
    ).rejects.toThrow("读取不完整");
  }
});

it("reads an 8 TiB fully sparse layout using only a bounded directory", async () => {
  const b = new Uint8Array(MiB);
  b.set(fixture().subarray(0, 21 * 512));
  set32(b, 8, 1);
  set64(b, 12, 2 ** 43 / 512);
  set64(b, 20, MiB / 512);
  set64(b, 48, 0);
  set64(b, 56, 21);
  set64(b, 64, MiB / 512);
  editDescriptor(b, (s) => s.replace("RW 16384", `RW ${2 ** 43 / 512}`));
  const input = memoryFile(b),
    image = await openVmdk(input.file);
  expect((await image.read(2 ** 43 - MiB, MiB)).some(Boolean)).toBe(false);
  await image.verify();
  expect(input.reads.reduce((n, [a, z]) => n + z - a, 0)).toBeLessThan(
    160 * 1024,
  );
});

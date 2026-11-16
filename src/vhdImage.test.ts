import { expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { openVhd } from "./vhdImage";
import {
  MiB,
  be32,
  be64,
  seal,
  fixedVhd,
  dynamicVhd,
  logicalDisk,
  memoryFile,
} from "../tests/fixtures/vhd-memory";
const u32 = (b: Uint8Array, at: number) => new DataView(b.buffer).getUint32(at);
it("decodes independent QEMU fixed/dynamic fixtures with the same MBR and EBR bytes", async () => {
  const fixture = (kind: string) =>
    gunzipSync(
      readFileSync(
        new URL(`../tests/fixtures/vhd-qemu-${kind}.gz`, import.meta.url),
      ),
    );
  const raw = fixture("raw");
  for (const kind of ["fixed", "dynamic"]) {
    const input = memoryFile(fixture(kind)),
      image = await openVhd(input.file);
    expect(image.size).toBe(raw.length);
    for (const [at, length] of [
      [0, MiB],
      [raw.length - MiB, MiB],
      [MiB, 512],
      [4 * MiB, 512],
      [2 * MiB, 512],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === raw[at + i]),
      ).toBe(true);
    await image.verify();
    expect(input.reads.reduce((n, [a, b]) => n + b - a, 0)).toBeLessThan(
      3 * MiB,
    );
  }
});
it("reads exact fixed and dynamically mapped bytes without including the container footer", async () => {
  const raw = logicalDisk();
  for (const bytes of [fixedVhd(raw), dynamicVhd(raw)]) {
    const before = bytes.slice();
    const input = memoryFile(bytes),
      image = await openVhd(input.file);
    expect(image.size).toBe(raw.length);
    for (const [at, count] of [
      [0, MiB],
      [raw.length - MiB, MiB],
      [2 * MiB - 257, 1031],
      [4 * MiB + 29, 511],
    ])
      expect(
        (await image.read(at, count)).every((b, i) => b === raw[at + i]),
      ).toBe(true);
    await image.verify();
    expect(bytes.every((b, i) => b === before[i])).toBe(true);
    expect(input.reads.reduce((n, [a, b]) => n + b - a, 0)).toBeLessThan(
      3 * MiB,
    );
  }
});
it("zero-fills absent blocks and MSB-first absent sectors, even with stale nonzero stored bytes", async () => {
  const raw = logicalDisk(),
    bytes = dynamicVhd(raw, true);
  const first = u32(bytes, 1536) * 512;
  bytes[first] = 0x81; // only sectors 0 and 7 are present
  const image = await openVhd(memoryFile(bytes).file);
  const got = await image.read(0, 4096);
  expect(got.subarray(0, 512)).toEqual(raw.subarray(0, 512));
  expect(got.subarray(512, 7 * 512).some(Boolean)).toBe(false);
  expect(got.subarray(7 * 512)).toEqual(raw.subarray(7 * 512, 4096));
  expect((await image.read(2 * MiB, 512)).some(Boolean)).toBe(false);
  expect(await image.read(4 * MiB, 512)).toEqual(
    raw.subarray(4 * MiB, 4 * MiB + 512),
  );
});
it("supports multi-sector bitmaps and a partial last logical block", async () => {
  const raw = new Uint8Array(12 * MiB);
  raw.fill(0x57, 8 * MiB);
  const bytes = dynamicVhd(raw, false, 8 * MiB);
  const image = await openVhd(memoryFile(bytes).file);
  expect(await image.read(raw.length - 1024, 1024)).toEqual(raw.slice(-1024));
  expect(await image.read(8 * MiB - 1024, 2048)).toEqual(
    raw.slice(8 * MiB - 1024, 8 * MiB + 1024),
  );
});
it("rejects bad footer checksums, saved states, versions, differencing parents and unsafe capacities", async () => {
  const good = fixedVhd();
  for (const mutate of [
    (b: Uint8Array) => {
      b[0] ^= 1;
    },
    (b: Uint8Array) => {
      b[84] = 1;
    },
    (b: Uint8Array) => {
      be32(b, 12, 0x20000);
    },
    (b: Uint8Array) => {
      be32(b, 8, 6);
    },
    (b: Uint8Array) => {
      be32(b, 60, 4);
    },
    (b: Uint8Array) => {
      be64(b, 48, 2n ** 63n);
    },
    (b: Uint8Array) => {
      be64(b, 48, 8 * MiB - 1);
    },
    (b: Uint8Array) => {
      be64(b, 48, 1);
    },
    (b: Uint8Array) => {
      be64(b, 16, 512);
    },
    (b: Uint8Array) => {
      b[90] = 1;
    },
    (b: Uint8Array) => {
      b.set(new TextEncoder().encode("qemu"), 28);
    },
  ]) {
    const b = good.slice(),
      f = b.subarray(b.length - 512);
    mutate(f);
    seal(f, 64);
    await expect(openVhd(memoryFile(b).file)).rejects.toThrow();
  }
  good[good.length - 450] ^= 1;
  await expect(openVhd(memoryFile(good).file)).rejects.toThrow("校验和");
});
it("rejects inconsistent duplicate footers and malformed dynamic headers before data reads", async () => {
  const good = dynamicVhd();
  const inconsistent = good.slice();
  inconsistent[68] ^= 1;
  seal(inconsistent.subarray(0, 512), 64);
  await expect(openVhd(memoryFile(inconsistent).file)).rejects.toThrow(
    "首尾副本",
  );
  for (const mutate of [
    (h: Uint8Array) => {
      h[0] ^= 1;
    },
    (h: Uint8Array) => {
      be32(h, 24, 2);
    },
    (h: Uint8Array) => {
      be64(h, 8, 0);
    },
    (h: Uint8Array) => {
      h[40] = 1;
    },
    (h: Uint8Array) => {
      h[576] = 1;
    },
    (h: Uint8Array) => {
      be64(h, 16, 512);
    },
    (h: Uint8Array) => {
      be64(h, 16, 1537);
    },
    (h: Uint8Array) => {
      be64(h, 16, 2n ** 63n);
    },
    (h: Uint8Array) => {
      be32(h, 28, 1);
    },
    (h: Uint8Array) => {
      be32(h, 28, 2 ** 30);
    },
    (h: Uint8Array) => {
      be32(h, 32, 0);
    },
    (h: Uint8Array) => {
      be32(h, 32, 3 * MiB);
    },
  ]) {
    const b = good.slice(),
      h = b.subarray(512, 1536);
    mutate(h);
    seal(h, 36);
    const input = memoryFile(b);
    await expect(openVhd(input.file)).rejects.toThrow();
    expect(input.reads.length).toBeLessThanOrEqual(3);
  }
});
it("rejects duplicate, overlapping, metadata-aliased and out-of-file data blocks", async () => {
  const good = dynamicVhd();
  for (const entry of [
    0,
    1,
    3,
    u32(good, 1540),
    u32(good, 1540) + 1,
    0xfffffffe,
  ]) {
    const b = good.slice();
    be32(b, 1536, entry);
    await expect(openVhd(memoryFile(b).file)).rejects.toThrow(/重叠|越界/);
  }
});
it("detects metadata or bitmap changes before the import is accepted", async () => {
  for (const offset of [0, 512, 1536, dynamicVhd().length - 512]) {
    const b = dynamicVhd(),
      image = await openVhd(memoryFile(b).file);
    b[offset] ^= 1;
    await expect(image.verify()).rejects.toThrow("发生变化");
  }
  const b = dynamicVhd(),
    image = await openVhd(memoryFile(b).file);
  await image.read(0, 512);
  b[u32(b, 1536) * 512] ^= 1;
  await expect(image.verify()).rejects.toThrow("发生变化");
});
it("bounds logical reads and rejects incomplete source reads without fabricating bytes", async () => {
  for (const [at, count] of [
    [-1, 512],
    [0, MiB + 1],
    [8 * MiB, 1],
    [0, NaN],
    [0.5, 512],
  ]) {
    const image = await openVhd(memoryFile(fixedVhd()).file);
    await expect(image.read(at, count)).rejects.toThrow();
  }
  const image = await openVhd(memoryFile(fixedVhd()).file);
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  const bad = memoryFile(fixedVhd()).file;
  bad.slice = (() => ({
    arrayBuffer: async () => new ArrayBuffer(511),
  })) as File["slice"];
  await expect(openVhd(bad)).rejects.toThrow("读取不完整");
});

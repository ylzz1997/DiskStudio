import { expect, it } from "vitest";
import { openVhdChain } from "./vhdChain";
import { openVhdLayer, vhdBudget } from "./vhdImage";
import { MissingImageFilesError } from "./imageDependencies";
import {
  MiB,
  be32,
  be64,
  fixedVhd,
  logicalDisk,
} from "../tests/fixtures/vhd-memory";
import {
  chainBytes,
  chainFile,
  stamp,
  changeFooter,
  changeHeader,
  emptyChild,
} from "../tests/fixtures/vhd-chain-data";
const names = ["leaf.avhd", "middle.avhd", "base.vhd"];
const u32 = (b: Uint8Array, at: number) =>
  new DataView(b.buffer, b.byteOffset).getUint32(at);
it("merges an independent DiscUtils three-layer corpus including child zero sectors, inherited EBRs and partial boundaries", async () => {
  const expected = chainBytes("expected.raw"),
    inputs = names.map((n) => chainFile(n));
  const image = await openVhdChain(inputs[0].file, [
    inputs[2].file,
    inputs[1].file,
  ]);
  expect(image.chain).toEqual(names);
  expect(image.fileCount).toBe(3);
  expect(image.fileSize).toBe(inputs.reduce((n, i) => n + i.file.size, 0));
  for (const [at, n] of [
    [0, MiB],
    [7 * MiB, MiB],
    [MiB, 512],
    [4 * MiB, 512],
    [2 * MiB - 513, 1537],
    [8191, 1538],
  ])
    expect(
      (await image.read(at, n)).every((v, i) => v === expected[at + i]),
    ).toBe(true);
  expect((await image.read(8192, 512)).some(Boolean)).toBe(false);
  await image.verify();
  expect(
    inputs.flatMap((i) => i.reads).reduce((n, [a, b]) => n + b - a, 0),
  ).toBeLessThan(4 * MiB);
});
it("requests each missing parent and never fabricates a complete child disk without its chain", async () => {
  const child = chainFile(names[0]),
    middle = chainFile(names[1]);
  await expect(openVhdChain(child.file)).rejects.toMatchObject({
    kind: "parent",
    missing: [expect.stringContaining("middle.avhd")],
  });
  await expect(openVhdChain(child.file, [middle.file])).rejects.toMatchObject({
    missing: [expect.stringContaining("base.vhd")],
  });
  await expect((await openVhdLayer(child.file)).read(0, 512)).rejects.toThrow(
    "不完整",
  );
});
it("matches UUIDs rather than filenames or stored paths, including renamed same-name selected parents", async () => {
  const image = await openVhdChain(chainFile(names[0]).file, [
    chainFile(names[1], undefined, "renamed.vhd").file,
    chainFile(names[2], undefined, "renamed.vhd").file,
  ]);
  expect(image.chain).toEqual(["leaf.avhd", "renamed.vhd", "renamed.vhd"]);
  const expected = chainBytes("expected.raw");
  expect((await image.read(0, MiB)).every((v, i) => v === expected[i])).toBe(
    true,
  );
  // Absolute C:\SyntheticFixtures locators do not exist and are never resolved.
  expect(image.notes.join()).toContain("不会被访问");
});
it("rejects stale or unavailable parent file timestamps, identity/size mismatches and duplicate UUIDs", async () => {
  for (const value of [stamp + 1000, NaN, undefined]) {
    const middle = chainFile(names[1]).file;
    Object.assign(middle, { lastModified: value });
    await expect(
      openVhdChain(chainFile(names[0]).file, [
        middle,
        chainFile(names[2]).file,
      ]),
    ).rejects.toThrow("修改时间");
  }
  const size = chainBytes(names[1]);
  changeFooter(size, (f) => be64(f, 48, 7 * MiB));
  await expect(
    openVhdChain(chainFile(names[0]).file, [chainFile(names[1], size).file]),
  ).rejects.toThrow("容量");
  await expect(
    openVhdChain(chainFile(names[0]).file, [
      chainFile(names[1]).file,
      chainFile(names[1]).file,
    ]),
  ).rejects.toThrow("重复");
  await expect(
    openVhdChain(chainFile(names[0]).file, [chainFile(names[0]).file]),
  ).rejects.toThrow("循环");
  const wrong = chainBytes(names[1]);
  changeFooter(wrong, (f) => (f[68] ^= 1));
  await expect(
    openVhdChain(chainFile(names[0]).file, [chainFile(names[1], wrong).file]),
  ).rejects.toBeInstanceOf(MissingImageFilesError);
});
it("detects cycles, zero parent identities, a missing seventeenth layer and the selected-file cap", async () => {
  const middle = chainBytes(names[1]),
    leaf = chainBytes(names[0]);
  changeHeader(middle, (h) => h.set(leaf.subarray(68, 84), 40));
  await expect(
    openVhdChain(chainFile(names[0], leaf).file, [
      chainFile(names[1], middle).file,
    ]),
  ).rejects.toThrow("循环");
  const zero = chainBytes(names[0]);
  changeHeader(zero, (h) => h.fill(0, 40, 56));
  await expect(openVhdChain(chainFile(names[0], zero).file)).rejects.toThrow(
    "标识",
  );
  await expect(
    openVhdChain(
      chainFile(names[0]).file,
      Array(16).fill(chainFile(names[1]).file),
    ),
  ).rejects.toThrow("16");
  const base = chainBytes(names[2]),
    layers = [chainFile(names[2], base).file];
  let parentId = base.slice(68, 84);
  for (let i = 1; i <= 15; i++) {
    const b = emptyChild(i, parentId);
    layers.unshift(chainFile(`layer${i}.avhd`, b).file);
    parentId = b.slice(68, 84);
  }
  const valid = await openVhdChain(layers[0], layers.slice(1));
  expect(valid.fileCount).toBe(16);
  await valid.read(0, 512);
  await valid.verify();
  const seventeenth = chainFile("layer16.avhd", emptyChild(16, parentId)).file;
  await expect(openVhdChain(seventeenth, layers.slice(0, 15))).rejects.toThrow(
    "16",
  );
});
it("validates Windows locator reservations and rejects aliasing, overflows, unsupported codes and malformed names", async () => {
  for (const edit of [
    (h: Uint8Array) => be32(h, 60, 1),
    (h: Uint8Array) => (h[768] = 1),
    (h: Uint8Array) => be32(h, 576 + 6 * 24, 0x4d616358),
    (h: Uint8Array) => be32(h, 576 + 6 * 24 + 4, 1),
    (h: Uint8Array) => be32(h, 576 + 6 * 24 + 8, 513),
    (h: Uint8Array) => be32(h, 576 + 6 * 24 + 12, 1),
    (h: Uint8Array) => be64(h, 576 + 6 * 24 + 16, 512),
    (h: Uint8Array) => be64(h, 576 + 6 * 24 + 16, 2048),
    (h: Uint8Array) => be64(h, 576 + 6 * 24 + 16, 2n ** 63n),
    (h: Uint8Array) => {
      h[64] = 0xd8;
      h[65] = 0;
    },
  ]) {
    const b = chainBytes(names[0]);
    changeHeader(b, edit);
    await expect(openVhdLayer(chainFile(names[0], b).file)).rejects.toThrow();
  }
  const alias = chainBytes(names[0]);
  be32(alias, 1536, 4);
  await expect(openVhdLayer(chainFile(names[0], alias).file)).rejects.toThrow(
    "重叠",
  );
});
it("rechecks all used layers' footers, BAT, locator payloads and accessed bitmaps", async () => {
  for (const layer of [0, 1, 2])
    for (const location of [0, 512, 1536]) {
      const data = names.map(chainBytes),
        image = await openVhdChain(chainFile(names[0], data[0]).file, [
          chainFile(names[1], data[1]).file,
          chainFile(names[2], data[2]).file,
        ]);
      data[layer][location] ^= 1;
      await expect(image.verify()).rejects.toThrow("发生变化");
    }
  for (const at of [2048, u32(chainBytes(names[0]), 1536) * 512]) {
    const b = chainBytes(names[0]),
      image = await openVhdChain(
        chainFile(names[0], b).file,
        names.slice(1).map((n) => chainFile(n).file),
      );
    await image.read(0, MiB);
    b[at] ^= 1;
    await expect(image.verify()).rejects.toThrow("发生变化");
  }
});
it("supports fixed parents and bounds shared BAT, metadata, bitmap and I/O budgets", async () => {
  const raw = logicalDisk(),
    base = fixedVhd(raw),
    parentId = base.subarray(base.length - 512 + 68, base.length - 512 + 84);
  const child = emptyChild(1, parentId),
    image = await openVhdChain(chainFile("child.avhd", child).file, [
      chainFile("fixed.vhd", base).file,
    ]);
  expect((await image.read(0, MiB)).every((v, i) => v === raw[i])).toBe(true);
  await image.verify();
  for (const [key, limit] of [
    ["maps", 16 * MiB],
    ["metadata", 32 * MiB],
    ["bytes", 128 * MiB],
    ["calls", 100000],
    ["bitmaps", 16 * MiB],
  ] as const) {
    const budget = vhdBudget();
    budget[key] = limit;
    await expect(
      (async () => {
        const l = await openVhdLayer(chainFile(names[0]).file, budget);
        await l.read(8192, 512, async (_a, n) => new Uint8Array(n));
      })(),
    ).rejects.toThrow(/上限|资源/);
  }
});
it("does not read unused selected parent contents and bounds logical reads across the chain", async () => {
  const extra = chainFile("unused.vhd", fixedVhd()),
    image = await openVhdChain(chainFile(names[0]).file, [
      ...names.slice(1).map((n) => chainFile(n).file),
      extra.file,
    ]);
  expect(extra.reads).toEqual([[extra.file.size - 512, extra.file.size]]);
  expect(image.fileCount).toBe(3);
  for (const [at, n] of [
    [-1, 512],
    [0, 0],
    [NaN, 1],
    [0, 1.5],
    [8 * MiB, 1],
  ])
    await expect(image.read(at, n)).rejects.toThrow();
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
});

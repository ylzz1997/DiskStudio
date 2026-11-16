import { expect, it } from "vitest";
import { openVmdkSource, MissingVmdkFilesError } from "./vmdkBundle";
import { openVmdk, vmdkBudget } from "./vmdkImage";
import { parseVmdkDescriptor, vmdkNameKey } from "./vmdkDescriptor";
import { memoryFile, MiB } from "../tests/fixtures/vhd-memory";
import { raw, fixture, set64 } from "../tests/fixtures/vmdk-data";
import {
  bundleBytes,
  bundleFile,
  bundleNames,
} from "../tests/fixtures/vmdk-bundle-data";
const textFile = (text: string, name = "main.vmdk") =>
  memoryFile(new TextEncoder().encode(text), name).file;
const text = (name = "split-flat.vmdk") =>
  new TextDecoder().decode(bundleBytes(name));
const desc = (lines: string, type = "twoGbMaxExtentFlat") =>
  `version=1\nCID=12345678\nparentCID=ffffffff\ncreateType="${type}"\n${lines}\n`;

it("matches QEMU flat and split images including reads across physical files, offsets and both EBRs", async () => {
  const expected = raw();
  for (const names of Object.values(bundleNames)) {
    const inputs = names.map((name) => memoryFile(bundleBytes(name), name));
    const image = await openVmdkSource(
      inputs[0].file,
      inputs
        .slice(1)
        .reverse()
        .map((i) => i.file),
    );
    expect(image.size).toBe(expected.length);
    expect(image.fileCount).toBe(names.length);
    expect(image.fileSize).toBe(inputs.reduce((n, i) => n + i.file.size, 0));
    for (const [at, length] of [
      [0, MiB],
      [7 * MiB, MiB],
      [MiB, 512],
      [4 * MiB - 256, 1024],
    ])
      expect(
        (await image.read(at, length)).every((v, i) => v === expected[at + i]),
      ).toBe(true);
    await image.verify();
    expect(
      inputs.flatMap((i) => i.reads).reduce((n, [a, b]) => n + b - a, 0),
    ).toBeLessThan(5 * MiB);
  }
});
it("reports exactly missing companions before any selected data is read", async () => {
  const part = memoryFile(bundleBytes("flat0.bin"), "flat0.bin");
  await expect(
    openVmdkSource(bundleFile("split-flat.vmdk"), [part.file]),
  ).rejects.toMatchObject({ missing: ["flat1.bin"] });
  expect(part.reads).toEqual([]);
  await expect(
    openVmdkSource(bundleFile("split-flat.vmdk")),
  ).rejects.toBeInstanceOf(MissingVmdkFilesError);
  const unrelated = memoryFile(new Uint8Array(512), "unused.bin");
  const image = await openVmdkSource(bundleFile("split-flat.vmdk"), [
    part.file,
    bundleFile("flat1.bin"),
    unrelated.file,
  ]);
  await image.read(0, 512);
  await image.verify();
  expect(image.fileCount).toBe(3);
  expect(unrelated.reads).toEqual([]);
  expect(image.notes.join()).toContain("无关文件");
});
it("binds safe case-folded names and rejects ambiguous names and self references", async () => {
  const upper = memoryFile(
    bundleBytes("flat-flat.vmdk"),
    "FLAT-FLAT.VMDK",
  ).file;
  expect(
    (await openVmdkSource(bundleFile("flat.vmdk"), [upper])).fileCount,
  ).toBe(2);
  await expect(
    openVmdkSource(bundleFile("flat.vmdk"), [
      upper,
      bundleFile("flat-flat.vmdk"),
    ]),
  ).rejects.toThrow("重复");
  await expect(
    openVmdkSource(textFile(desc('RW 8192 FLAT "main.vmdk" 0'))),
  ).rejects.toThrow("自身");
  for (const name of [
    "../disk.vmdk",
    "C:\\disk.vmdk",
    "\\\\.\\PhysicalDrive1",
    "disk:stream",
    "nul.bin",
    "COM1",
    "lpt².vmdk",
    ".",
    "..",
    "a.",
    " a",
    "a/part",
    "a?",
    "a\u0000b",
  ])
    expect(() => vmdkNameKey(name)).toThrow();
  expect(vmdkNameKey("数据分卷.vmdk")).toBe("数据分卷.vmdk");
});
it("rejects parent chains, unsupported forms, malformed geometry and excessive extents without touching companions", async () => {
  const input = memoryFile(bundleBytes("flat0.bin"), "flat0.bin");
  for (const edit of [
    (s: string) => s.replace("ffffffff", "abcd1234"),
    (s: string) => s + '\nparentFileNameHint="parent.vmdk"',
    (s: string) => s.replace("twoGbMaxExtentFlat", "vmfs"),
    (s: string) => s.replace("RDONLY", "NOACCESS"),
    (s: string) => s.replace("8192", "0"),
    (s: string) => s.replace("8192", "99999999999999999999"),
    (s: string) => s.replace('"flat0.bin" 1', '"flat0.bin"'),
    (s: string) => s.replace(" FLAT ", " ZERO "),
    (s: string) => s + "\nVersion=1",
    (s: string) => s + '\nddb.encrypted="true"',
    (s: string) => s + '\nddb.logicalSectorSize="4096"',
  ])
    await expect(
      openVmdkSource(textFile(edit(text())), [input.file]),
    ).rejects.not.toBeInstanceOf(MissingVmdkFilesError);
  expect(input.reads).toEqual([]);
  expect(() =>
    parseVmdkDescriptor(
      new TextEncoder().encode(
        desc(Array(65).fill('RW 8192 FLAT "flat0.bin" 0').join("\n")),
      ),
    ),
  ).toThrow("64");
  await expect(
    openVmdkSource(memoryFile(new Uint8Array(65537), "large.vmdk").file),
  ).rejects.toThrow("64 KiB");
  await expect(
    openVmdkSource(bundleFile("flat.vmdk"), Array(65).fill(input.file)),
  ).rejects.toThrow("64");
});
it("allows disjoint ranges in a shared flat file but rejects overlapping and sparse aliases", async () => {
  const input = memoryFile(raw(), "data.bin").file;
  const image = await openVmdkSource(
    textFile(desc('RW 8192 FLAT "data.bin" 8192\nRW 8192 FLAT "data.bin" 0')),
    [input],
  );
  const result = await image.read(4 * MiB - 32, 64),
    expected = raw();
  expect(
    result.slice(0, 32).every((v, i) => v === expected[8 * MiB - 32 + i]),
  ).toBe(true);
  expect(result.slice(32).every((v, i) => v === expected[i])).toBe(true);
  expect(image.fileCount).toBe(2);
  await image.verify();
  await expect(
    openVmdkSource(
      textFile(desc('RW 8192 FLAT "data.bin" 0\nRW 8192 FLAT "data.bin" 1')),
      [input],
    ),
  ).rejects.toThrow("重叠");
  await expect(
    openVmdkSource(
      textFile(
        desc(
          'RW 8192 SPARSE "part0-s001.vmdk"\nRW 8192 SPARSE "part0-s001.vmdk"',
          "twoGbMaxExtentSparse",
        ),
      ),
      [bundleFile("part0-s001.vmdk")],
    ),
  ).rejects.toThrow("重复");
});
it("rejects truncated flat files, changed descriptors and accessed flat bytes", async () => {
  const b = bundleBytes("flat-flat.vmdk"),
    d = bundleBytes("flat.vmdk");
  await expect(
    openVmdkSource(memoryFile(d, "flat.vmdk").file, [
      memoryFile(b.subarray(0, b.length - 512), "flat-flat.vmdk").file,
    ]),
  ).rejects.toThrow("越界");
  for (const change of [
    () => {
      d[0] ^= 1;
    },
    () => {
      b[0] ^= 1;
    },
  ]) {
    const image = await openVmdkSource(memoryFile(d, "flat.vmdk").file, [
      memoryFile(b, "flat-flat.vmdk").file,
    ]);
    await image.read(0, 512);
    change();
    await expect(image.verify()).rejects.toThrow("发生变化");
    change();
  }
  const short = bundleFile("flat-flat.vmdk"),
    slice = short.slice.bind(short);
  short.slice = ((a: number, z: number) => slice(a, z - 1)) as File["slice"];
  const image = await openVmdkSource(bundleFile("flat.vmdk"), [short]);
  await expect(image.read(0, 512)).rejects.toThrow("读取不完整");
});
it("reuses sparse validation, checks extent capacity, and refuses a bare part as a standalone disk", async () => {
  const names = bundleNames.sparse;
  const b = bundleBytes(names[1]);
  const image = await openVmdkSource(bundleFile(names[0]), [
    memoryFile(b, names[1]).file,
    bundleFile(names[2]),
  ]);
  b[0] ^= 1;
  await expect(image.verify()).rejects.toThrow("发生变化");
  b[0] ^= 1;
  set64(b, 12, 8191);
  await expect(
    openVmdkSource(bundleFile(names[0]), [
      memoryFile(b, names[1]).file,
      bundleFile(names[2]),
    ]),
  ).rejects.toThrow("不一致");
  await expect(openVmdkSource(bundleFile(names[1]))).rejects.toThrow(
    "主描述文件",
  );
  await expect(
    openVmdkSource(bundleFile(names[0]), [
      memoryFile(fixture(), names[1]).file,
      bundleFile(names[2]),
    ]),
  ).rejects.toThrow("不一致");
});
it("supports sub-4 MiB sparse final extents and shares all sparse resource budgets", async () => {
  const tail = bundleBytes("part1-s001.vmdk"),
    head = bundleBytes("part0-s001.vmdk");
  // A fully sparse single-sector extent with no tables, inherited from a real header.
  const tiny = head.slice(0, 65536);
  set64(tiny, 12, 1);
  set64(tiny, 28, 0);
  set64(tiny, 36, 0);
  new DataView(tiny.buffer).setUint32(8, 1, true);
  set64(tiny, 48, 0);
  set64(tiny, 56, 1);
  tiny.fill(0, 512);
  const image = await openVmdkSource(
    textFile(
      desc('RW 8192 SPARSE "a"\nRW 1 SPARSE "b"', "twoGbMaxExtentSparse"),
    ),
    [memoryFile(head, "a").file, memoryFile(tiny, "b").file],
  );
  expect((await image.read(4 * MiB, 512)).some(Boolean)).toBe(false);
  await image.verify();
  for (const [key, limit] of [
    ["mapBytes", 32 * MiB],
    ["metaBytes", 66 * MiB],
    ["fileBytes", 160 * MiB],
    ["calls", 100000],
  ] as const) {
    const budget = vmdkBudget();
    budget[key] = limit;
    await expect(
      openVmdk(memoryFile(tail).file, undefined, { size: 4 * MiB, budget }),
    ).rejects.toThrow(/上限|资源/);
  }
});
it("bounds whole-disk reads and preserves standalone import behavior", async () => {
  const image = await openVmdkSource(bundleFile("flat.vmdk"), [
    bundleFile("flat-flat.vmdk"),
  ]);
  for (const [at, len] of [
    [-1, 512],
    [0, 0],
    [NaN, 1],
    [0, 0.5],
    [8 * MiB, 1],
  ])
    await expect(image.read(at, len)).rejects.toThrow();
  for (let i = 0; i < 4; i++) await image.read(0, MiB);
  await expect(image.read(0, 1)).rejects.toThrow("有限字节");
  const standalone = await openVmdkSource(
    memoryFile(fixture(), "single.vmdk").file,
  );
  expect(standalone.format).toContain("单文件");
});

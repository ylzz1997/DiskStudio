import { expect, it } from "vitest";
import { crc32c, openVhdx, openVhdxChain } from "./vhdxImage";
import { MissingImageFilesError } from "./imageDependencies";
import { browseReader } from "./imageBrowserSource";
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
  logEntry,
  markActiveLog,
  seal,
} from "../tests/fixtures/vhdx-data";

const BASE_ID = "6270ef37-75b0-4c33-9ee9-3d634790f431";
const MID_ID = "762bfa65-871e-4dbd-a853-20b718f963da";
const TOP_ID = "50f19e2a-2a6d-4f96-8fc9-781f276de22b";
const PARENT_ITEM = META + 128 * K;

function setDataId(bytes: Uint8Array, id: string) {
  const [a, b, c, d, e] = id.split("-");
  const fromHex = (s: string) =>
    Uint8Array.from(s.match(/../g)!, (pair) => parseInt(pair, 16));
  const binary = [
    ...fromHex(a).reverse(),
    ...fromHex(b).reverse(),
    ...fromHex(c).reverse(),
    ...fromHex(d + e),
  ];
  for (const header of [64 * K, 128 * K]) {
    bytes.set(binary, header + 32);
    seal(bytes, header, 4096);
  }
}

function difference(
  sector: 512 | 4096,
  id: string,
  parentId: string,
  entries: {
    block: number;
    state: 2 | 6 | 7;
    payload?: number;
    sectors?: number[];
  }[],
  hints: { name?: string; alternateId?: string; relativeName?: string } = {},
) {
  // The independent QEMU-generated header/regions provide 1 MiB data blocks.
  // Write a synthetic parent locator and a complete final bitmap BAT slot.
  const bytes = new Uint8Array((5 + entries.length) * MiB);
  bytes.set(
    fixture(sector === 4096 ? "gpt4k" : "dynamic").subarray(0, 4 * MiB),
  );
  setDataId(bytes, id);
  le32(bytes, item(bytes, 0) + 4, 2);
  const locator = bytes.subarray(PARENT_ITEM, PARENT_ITEM + K);
  locator.set([
    0xb7, 0xef, 0x4a, 0xb0, 0x9e, 0xd1, 0x81, 0x4a, 0xb7, 0x89, 0x25, 0xb8,
    0xe9, 0x44, 0x59, 0x13,
  ]);
  const fields = [
    [
      "absolute_win32_path",
      "\\\\?\\Z:\\non-selected\\" +
        (hints.name ?? "deliberately-unrelated.avhdx"),
    ],
    ["parent_linkage", "{" + parentId.toUpperCase() + "}"],
    [
      "parent_linkage2",
      "{" + (hints.alternateId ?? "00000000-0000-0000-0000-000000000000") + "}",
    ],
  ];
  if (hints.relativeName)
    fields.push(["relative_path", "..\\" + hints.relativeName]);
  locator[18] = fields.length;
  const view = new DataView(
    locator.buffer,
    locator.byteOffset,
    locator.byteLength,
  );
  let offset = 20 + fields.length * 12;
  fields.forEach((pair, i) =>
    pair.forEach((value, j) => {
      view.setUint32(20 + i * 12 + j * 4, offset, true);
      view.setUint16(28 + i * 12 + j * 2, value.length * 2, true);
      for (const unit of value.split("")) {
        view.setUint16(offset, unit.charCodeAt(0), true);
        offset += 2;
      }
    }),
  );
  bytes[META + 10] = 6;
  const descriptor = META + 32 + 5 * 32;
  bytes.set(
    [
      0x2d, 0x5f, 0xd3, 0xa8, 0x0b, 0xb3, 0x4d, 0x45, 0xab, 0xf7, 0xd3, 0xd8,
      0x48, 0x34, 0xab, 0x0c,
    ],
    descriptor,
  );
  le32(bytes, descriptor + 16, PARENT_ITEM - META);
  le32(bytes, descriptor + 20, offset);
  le32(bytes, descriptor + 24, 4);
  bytes.fill(0, BAT, BAT + MiB);
  const bitmap = 4 * MiB;
  const bitmapEntry = BAT + 8 * sector * 8;
  entries.forEach(({ block, state, payload, sectors }, i) => {
    const fileAt = state === 2 ? 0 : (5 + i) * MiB;
    le64(bytes, BAT + block * 8, fileAt + state);
    if (fileAt) bytes.fill(payload!, fileAt, fileAt + MiB);
    for (const number of sectors ?? []) {
      const bit = block * (MiB / sector) + number;
      bytes[bitmap + (bit >>> 3)] |= 1 << (bit & 7);
    }
    if (state === 7) le64(bytes, bitmapEntry, bitmap + 6);
  });
  return bytes;
}

it.each([512, 4096] as const)(
  "reads complete %i-byte-sector VHDX chains through retained files",
  async (sector) => {
    const base = fixture(sector === 4096 ? "gpt4k" : "dynamic");
    setDataId(base, BASE_ID);
    const middle = difference(sector, MID_ID, BASE_ID, [
      { block: 1, state: 7, payload: 0xa1, sectors: [0, 1, MiB / sector - 1] },
      { block: 4, state: 6, payload: 0xc1 },
    ]);
    const latest = difference(sector, TOP_ID, MID_ID, [
      { block: 1, state: 7, payload: 0xb2, sectors: [1, 2] },
      { block: 4, state: 2 },
      { block: 5, state: 6, payload: 0xd2 },
    ]);
    const files = [
      { id: "top", name: "snapshot.AVHDX", bytes: latest },
      { id: "base", name: "renamed.vhdx", bytes: base },
      { id: "mid", name: "middle.avhdx", bytes: middle },
    ];
    const group = {
      id: "group",
      files: files.map(({ id, name, bytes }) => ({
        id,
        name,
        size: bytes.length,
        lastModified: 0,
      })),
    };
    const image = await browseReader(
      group,
      { kind: "disk", sector },
      async (groupId, fileId, at, count) => {
        expect(groupId).toBe(group.id);
        const file = files.find((file) => file.id === fileId)!;
        expect(at + count).toBeLessThanOrEqual(file.bytes.length);
        return file.bytes.slice(at, at + count);
      },
    );
    expect(image.chain).toEqual([
      "snapshot.AVHDX",
      "middle.avhdx",
      "renamed.vhdx",
    ]);
    expect(image.fileSize).toBe(latest.length + middle.length + base.length);
    expect(image.sectorSize).toBe(sector);
    const expected = rawFixture(sector === 4096);
    expected.fill(0xa1, MiB, MiB + sector);
    expected.fill(0xb2, MiB + sector, MiB + 3 * sector);
    expected.fill(0xa1, 2 * MiB - sector, 2 * MiB);
    expected.fill(0, 4 * MiB, 5 * MiB);
    expected.fill(0xd2, 5 * MiB, 6 * MiB);
    for (let at = 0; at < expected.length; at += MiB)
      expect(
        Buffer.from(await image.read(at, MiB)).equals(
          expected.subarray(at, at + MiB),
        ),
      ).toBe(true);
    expect(
      Buffer.from(await image.read(MiB - 1, 3 * sector + 2)).equals(
        expected.subarray(MiB - 1, MiB + 3 * sector + 1),
      ),
    ).toBe(true);
    await image.verify();
    latest[4 * MiB + MiB / sector / 8] ^= 1;
    await expect(image.verify()).rejects.toThrow("扇区位图发生变化");
  },
);

it("rejects wrong, missing, ambiguous or cyclic VHDX parents and damaged mappings", async () => {
  const file = (bytes: Uint8Array, name = "layer.vhdx") =>
    memoryFile(bytes, name).file;
  const base = fixture();
  setDataId(base, BASE_ID);
  const latest = difference(512, TOP_ID, BASE_ID, [
    { block: 1, state: 7, payload: 0xa1, sectors: [0] },
  ]);
  const leaf = file(latest, "leaf.avhdx");
  await expect(openVhdxChain(leaf, [])).rejects.toMatchObject({
    constructor: MissingImageFilesError,
    kind: "parent",
    missing: ["deliberately-unrelated.avhdx"],
  });
  await expect(
    openVhdxChain(leaf, [file(base), file(base, "LAYER.VHDX")]),
  ).rejects.toThrow("重复");
  const wrong = fixture();
  setDataId(wrong, MID_ID);
  await expect(openVhdxChain(leaf, [file(wrong)])).rejects.toThrow("父盘文件");
  await expect(
    openVhdxChain(leaf, [file(base), file(wrong, "extra.vhdx")]),
  ).rejects.toThrow("不属于");
  const mismatched = fixture("gpt4k");
  setDataId(mismatched, BASE_ID);
  await expect(openVhdxChain(leaf, [file(mismatched)])).rejects.toThrow(
    "扇区大小",
  );
  const loop = difference(512, BASE_ID, TOP_ID, []);
  await expect(openVhdxChain(leaf, [file(loop)])).rejects.toThrow("循环");
  for (const [target, state, message] of [
    [BAT + 8 * 512 * 8, 0, "缺少扇区位图"],
    [BAT + 8 * 512 * 8, 5 * MiB + 6, "重叠"],
    [BAT + 8 * 512 * 8, 3 * MiB + 6, "元数据重叠"],
    [BAT + 8 * 8, 2, "无效"],
  ] as const) {
    const broken = latest.slice();
    le64(broken, target, state);
    await expect(openVhdxChain(file(broken), [file(base)])).rejects.toThrow(
      message,
    );
  }
  const malformed = latest.slice();
  le32(malformed, PARENT_ITEM + 20, 1);
  await expect(openVhdxChain(file(malformed), [file(base)])).rejects.toThrow(
    "字段越界",
  );
  const merging = difference(512, TOP_ID, BASE_ID, [], { alternateId: MID_ID });
  await expect(openVhdxChain(file(merging), [file(base)])).rejects.toThrow(
    "未完成合并",
  );
});
it("resolves unchanged Windows snapshots that share a data-write GUID", async () => {
  const baseBytes = fixture("gpt4k");
  setDataId(baseBytes, BASE_ID);
  const base = memoryFile(baseBytes, "base.vhdx").file;
  const middle = memoryFile(
    difference(4096, BASE_ID, BASE_ID, [], { name: "base.vhdx" }),
    "middle.avhdx",
  ).file;
  const top = memoryFile(
    difference(4096, BASE_ID, BASE_ID, [], { name: "middle.avhdx" }),
    "latest.avhdx",
  ).file;
  await expect(openVhdxChain(top, [base])).rejects.toMatchObject({
    kind: "parent",
    missing: ["middle.avhdx"],
  });
  const image = await openVhdxChain(top, [base, middle]);
  expect(image.chain).toEqual(["latest.avhdx", "middle.avhdx", "base.vhdx"]);
  expect(await image.read(4096, 4096)).toEqual(
    rawFixture(true).subarray(4096, 8192),
  );
  const ambiguous = memoryFile(
    difference(4096, BASE_ID, BASE_ID, [], {
      name: "middle.avhdx",
      relativeName: "base.vhdx",
    }),
    "ambiguous.avhdx",
  ).file;
  await expect(openVhdxChain(ambiguous, [base, middle])).rejects.toThrow(
    "无法确定读取顺序",
  );
});

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
  expect(input.reads).toEqual([
    [0, 8],
    [64 * K, 64 * K + 4096],
    [128 * K, 128 * K + 4096],
    [MiB, 2 * MiB],
  ]);
  oldDirty[128 * K + 48] = 0;
  oldDirty[128 * K + 66] = 2;
  seal(oldDirty, 128 * K, 4096);
  await expect(openVhdx(memoryFile(oldDirty).file)).rejects.toThrow(
    "当前头部版本",
  );
});

it("reads the newest complete VHDX log sequence across the ring boundary", async () => {
  const bytes = fixture();
  const sector = 4096;
  const tail = MiB - sector;
  const sequence = 0x1020304050n;
  const bat = bytes.slice(BAT, BAT + sector);
  const firstRegion = bytes.slice(192 * K, 192 * K + sector);
  const secondRegion = bytes.slice(256 * K, 256 * K + sector);
  bytes.fill(0, BAT, BAT + sector);
  bytes[192 * K] = 0;
  bytes[256 * K] = 0;
  markActiveLog(bytes);
  // Older complete entry and newer incomplete entry must both be ignored.
  logEntry(bytes, tail - sector, tail - sector, sequence - 1n, [{ target: 0, data: sector }]);
  logEntry(bytes, tail, tail, sequence, [{ target: BAT, data: bat }]);
  logEntry(bytes, sector, tail, sequence + 1n, [
    { target: BAT, data: sector },
    ...Array.from({ length: 125 }, () => ({ target: META + MiB - sector, data: sector })),
    { target: BAT, data: bat },
    { target: 192 * K, data: firstRegion },
    { target: 256 * K, data: secondRegion },
  ]);
  logEntry(bytes, 10 * sector, 11 * sector, sequence + 2n, [{ target: BAT, data: sector }]);
  const before = bytes.slice();
  const image = await openVhdx(memoryFile(bytes).file, true);
  expect(image.warnings).toContain("已在内存中回放 VHDX 容器日志，源镜像保持只读。");
  const expected = rawFixture();
  for (let at = 0; at < image.size; at += MiB)
    expect(Buffer.from(await image.read(at, MiB)).equals(expected.subarray(at, at + MiB))).toBe(true);
  await image.verify();
  expect(Buffer.from(bytes).equals(before)).toBe(true);
  bytes[MiB] ^= 1;
  await expect(image.verify()).rejects.toThrow("发生变化");
});

it.each([512, 4096] as const)("replays BAT and bitmap updates in a %i-byte-sector AVHDX", async (sector) => {
  const base = fixture(sector === 4096 ? "gpt4k" : "dynamic");
  setDataId(base, BASE_ID);
  const child = difference(sector, TOP_ID, BASE_ID, [
    { block: 1, state: 7, payload: 0x79, sectors: [1, 3] },
  ]);
  const bat = child.slice(BAT, BAT + 4096);
  const map = child.slice(4 * MiB, 4 * MiB + 4096);
  child.fill(0, BAT, BAT + 4096);
  child.fill(0, 4 * MiB, 4 * MiB + 4096);
  markActiveLog(child);
  logEntry(child, 0, 0, 19n, [
    { target: BAT, data: bat },
    { target: 4 * MiB, data: map },
  ]);
  const image = await openVhdxChain(memoryFile(child, "latest.avhdx").file, [memoryFile(base, "base.vhdx").file]);
  expect(image.warnings).toHaveLength(1);
  const expected = rawFixture(sector === 4096);
  expected.fill(0x79, MiB + sector, MiB + 2 * sector);
  expected.fill(0x79, MiB + 3 * sector, MiB + 4 * sector);
  expect(await image.read(MiB - 3, sector * 4 + 5)).toEqual(expected.subarray(MiB - 3, MiB + sector * 4 + 2));
  await image.verify();
});

it("uses the durable-length guard and models a committed file extension without writes", async () => {
  const bytes = fixture();
  const bat = bytes.slice(BAT, BAT + 4096);
  le64(bat, 3 * 8, bytes.length + 6);
  markActiveLog(bytes);
  logEntry(bytes, 0, 0, 1n, [{ target: BAT, data: bat }], bytes.length, bytes.length + MiB);
  const image = await openVhdx(memoryFile(bytes).file);
  expect(await image.read(3 * MiB + 17, 511)).toEqual(new Uint8Array(511));
  await image.verify();
  logEntry(bytes, 0, 0, 1n, [{ target: BAT, data: bat }], bytes.length + MiB, bytes.length + MiB);
  await expect(openVhdx(memoryFile(bytes).file)).rejects.toThrow("已持久化长度");
});

it("rejects ambiguous, torn, unsupported, and misdirected VHDX log records", async () => {
  const bytes = fixture();
  markActiveLog(bytes);
  logEntry(bytes, 0, 0, 1n, [{ target: BAT + 4096, data: 4096 }]);
  logEntry(bytes, 4096, 4096, 1n, []);
  await expect(openVhdx(memoryFile(bytes).file)).rejects.toThrow("同序号");
  bytes[MiB + 4096] = 0;
  const bat = bytes.slice(BAT, BAT + 4096);
  logEntry(bytes, 0, 0, 2n, [{ target: BAT, data: bat }]);
  bytes[MiB + 4096 + 4092] ^= 1;
  seal(bytes, MiB, 8192);
  await expect(openVhdx(memoryFile(bytes).file)).rejects.toThrow("完整序列");
  for (const [target, error] of [
    [64 * K, "头部或日志"],
    [MiB, "头部或日志"],
    [Number(get64(bytes, BAT) & ~7n), "有效载荷"],
    [2 ** 44, "超出支持的文件范围"],
  ] as const) {
    logEntry(bytes, 0, 0, 2n, [{ target, data: 4096 }]);
    await expect(openVhdx(memoryFile(bytes).file)).rejects.toThrow(error);
  }
  bytes[128 * K + 64] = 1;
  seal(bytes, 128 * K, 4096);
  await expect(openVhdx(memoryFile(bytes).file)).rejects.toThrow("版本或大小");
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

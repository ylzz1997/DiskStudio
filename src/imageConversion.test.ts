import { expect, it, vi } from "vitest";
import { createHash } from "node:crypto";
import {
  conversionOptions,
  conversionFrame,
  convertImage,
  type ConversionPort,
  type ImageConversionReport,
} from "./imageConversion";
import { openVhdLayer, vhdBudget } from "./vhdImage";
import { openVhdChain } from "./vhdChain";
import { openVhdx } from "./vhdxImage";
import { openVdi } from "./vdiImage";
import { openVmdk } from "./vmdkImage";
import { openParallels } from "./parallelsImage";
import type { LogicalImage } from "./imageSource";
import {
  dynamicVhd,
  fixedVhd,
  logicalDisk,
  memoryFile,
  MiB,
} from "../tests/fixtures/vhd-memory";
import { vdiBytes } from "../tests/fixtures/vdi-memory";
import {
  fixture as vhdx,
  rawFixture as vhdxRaw,
} from "../tests/fixtures/vhdx-data";
import {
  fixture as vmdk,
  raw as vmdkRaw,
  inflate,
} from "../tests/fixtures/vmdk-data";
import {
  fixture as parallels,
  raw as parallelsRaw,
} from "../tests/fixtures/parallels-data";
import { chainBytes, chainFile } from "../tests/fixtures/vhd-chain-data";
const digest = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");
async function verifyAll(image: LogicalImage, expected: Uint8Array) {
  const hash = createHash("sha256");
  await image.verify();
  for (let at = 0; at < image.size; at += MiB) {
    const length = Math.min(MiB, image.size - at);
    const b = await image.read(at, length);
    expect(b.length).toBe(length);
    hash.update(b);
  }
  await image.verify();
  expect(hash.digest("hex")).toBe(digest(expected));
  await expect(image.read(0, 512)).rejects.toThrow();
}
it("streams complete RAW-equivalent contents from each existing container decoder", async () => {
  const raw = logicalDisk();
  for (const bytes of [
    fixedVhd(raw),
    dynamicVhd(raw),
    dynamicVhd(raw, false, 512),
  ])
    await verifyAll(
      await openVhdLayer(memoryFile(bytes).file, vhdBudget(true)),
      raw,
    );
  await verifyAll(await openVdi(memoryFile(vdiBytes(raw)).file, true), raw);
  await verifyAll(await openVhdx(memoryFile(vhdx()).file, true), vhdxRaw());
  await verifyAll(
    await openVhdx(memoryFile(vhdx("gpt4k")).file, true),
    vhdxRaw(true),
  );
  for (const kind of ["sparse", "zero", "stream", "footer"])
    await verifyAll(
      await openVmdk(memoryFile(vmdk(kind)).file, inflate, undefined, true),
      vmdkRaw(),
    );
  for (const kind of ["extended", "legacy"])
    await verifyAll(
      await openParallels(memoryFile(parallels(kind)).file, true),
      parallelsRaw(),
    );
}, 30000);
it("flattens all inherited VHD data while retaining the default inspection read ceiling", async () => {
  const input = () =>
    ["leaf.avhd", "middle.avhd", "base.vhd"].map((n) => chainFile(n).file);
  let [leaf, ...parents] = input();
  await verifyAll(
    await openVhdChain(leaf, parents, true),
    chainBytes("expected.raw"),
  );
  const limited = await openVdi(memoryFile(vdiBytes()).file);
  for (let i = 0; i < 4; i++) await limited.read(i * MiB, MiB);
  await expect(limited.read(4 * MiB, MiB)).rejects.toThrow("有限字节");
});
function reader(size = 4 * MiB): LogicalImage {
  return {
    size,
    sectorSize: 512,
    format: "test",
    warnings: [],
    notes: [],
    verify: vi.fn(async () => {}),
    read: vi.fn(async (_at, n) => new Uint8Array(n)),
  };
}
function port() {
  const job: ImageConversionReport = {
    id: "conversion",
    options: { format: "raw", size: 4 * MiB, sector: 512 },
    state: "receiving",
    message: "",
    received: 0,
    sourceVerified: 0,
    verified: 0,
    outputName: "out.img",
    partialName: "out.partial",
    result: null,
  };
  const p: ConversionPort = {
    begin: vi.fn(async () => job),
    push: vi.fn(async (_id, at, b) => at + b.length),
    finish: vi.fn(async () => ({ ...job, state: "completed" })),
    cancel: vi.fn(async () => {}),
  };
  return p;
}
it("opens a fresh source for the second pass and sends exactly two ordered streams before finish", async () => {
  const open = vi.fn(async () => reader(4 * MiB + 512)),
    p = port();
  const result = await convertImage(
    open,
    "raw",
    p,
    new AbortController().signal,
    () => {},
  );
  expect(result?.state).toBe("completed");
  expect(open).toHaveBeenCalledTimes(2);
  const calls = vi.mocked(p.push).mock.calls;
  expect(calls.length).toBe(10);
  expect(calls.map((c) => [c[1], c[2].length, c[3]])).toEqual(
    [false, true].flatMap((check) =>
      [0, 1, 2, 3, 4].map((i) => [i * MiB, i === 4 ? 512 : MiB, check]),
    ),
  );
  expect(p.finish).toHaveBeenCalledOnce();
  expect(p.cancel).not.toHaveBeenCalled();
});
it("rejects source geometry changes and short reads without publishing output", async () => {
  for (const short of [false, true]) {
    let pass = 0;
    const open = async () => {
      const r = reader(pass++ === 0 ? 4 * MiB : 5 * MiB);
      if (short) r.read = async () => new Uint8Array(2);
      return r;
    };
    const p = port();
    await expect(
      convertImage(open, "raw", p, new AbortController().signal, () => {}),
    ).rejects.toThrow();
    expect(p.finish).not.toHaveBeenCalled();
    expect(p.cancel).toHaveBeenCalledOnce();
  }
});
it("canceling destination selection reads no source data; stopping never starts a new job or finishes", async () => {
  const r = reader(),
    p = port();
  vi.mocked(p.begin).mockResolvedValue(null);
  expect(
    await convertImage(
      async () => r,
      "raw",
      p,
      new AbortController().signal,
      () => {},
    ),
  ).toBe(null);
  expect(r.read).not.toHaveBeenCalled();
  const q = port(),
    c = new AbortController();
  vi.mocked(q.push).mockImplementation(async (_id, at, b) => {
    c.abort();
    return at + b.length;
  });
  await expect(
    convertImage(
      async () => reader(),
      "raw",
      q,
      c.signal,
      () => {},
    ),
  ).rejects.toThrow("停止");
  expect(q.begin).toHaveBeenCalledOnce();
  expect(q.push).toHaveBeenCalledOnce();
  expect(q.finish).not.toHaveBeenCalled();
  expect(q.cancel).toHaveBeenCalledOnce();
});
it("does not reinterpret 4Kn as 512-byte or round an unaligned logical size", () => {
  const r = reader();
  r.sectorSize = 4096;
  expect(conversionOptions(r, "raw").sector).toBe(4096);
  for (const f of [
    "vdiDynamic",
    "vdiFixed",
    "vhdFixed",
    "vhdDynamic",
    "vmdkSparse",
    "parallels",
  ] as const)
    expect(() => conversionOptions(r, f)).toThrow("512");
  for (const f of ["vhdxDynamic", "vhdxFixed"] as const)
    expect(conversionOptions(r, f).sector).toBe(4096);
  r.size++;
  expect(() => conversionOptions(r, "raw")).toThrow("对齐");
});
it("encodes bounded binary frames without JSON expansion or losing byte/offset precision", () => {
  const id = "12345678-1234-4321-8899-aabbccddeeff",
    bytes = new Uint8Array(MiB);
  bytes[0] = 0xff;
  bytes[MiB - 1] = 0x81;
  for (const check of [false, true]) {
    const offset = 2 ** 42,
      frame = conversionFrame(id, offset, bytes, check);
    expect(frame.length).toBe(MiB + 25);
    expect(frame[0]).toBe(check ? 1 : 0);
    expect(Buffer.from(frame.subarray(1, 17)).toString("hex")).toBe(
      id.replaceAll("-", ""),
    );
    expect(new DataView(frame.buffer).getBigUint64(17, true)).toBe(
      BigInt(offset),
    );
    expect(Buffer.from(frame.subarray(25)).equals(Buffer.from(bytes))).toBe(true);
  }
  for (const offset of [-1, 1, 512, 2 ** 43, NaN])
    expect(() => conversionFrame(id, offset, bytes, false)).toThrow();
  for (const bad of ["bad", "00000000-0000-0000-0000-000000000000"])
    expect(() => conversionFrame(bad, 0, bytes, false)).toThrow();
  for (const length of [0, MiB + 1])
    expect(() =>
      conversionFrame(id, 0, new Uint8Array(length), false),
    ).toThrow();
});

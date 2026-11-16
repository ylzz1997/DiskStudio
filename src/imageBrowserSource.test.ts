import { expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { gunzipSync, inflateSync } from "node:zlib";
import { createHash } from "node:crypto";
import {
  browseReader,
  retainedImageFile,
  imageReplyFrame,
  ImageReadServer,
  MiB,
  type NativeImageGroup,
  type SourceRead,
  type ImageReadEvent,
} from "./imageBrowserSource";
import type { LogicalImage } from "./imageSource";
import { chainBytes, stamp } from "../tests/fixtures/vhd-chain-data";
import { bundleBytes, bundleNames } from "../tests/fixtures/vmdk-bundle-data";

// Compressed grain IPC is replaced only with the independent bounded zlib
// implementation. All container parsing and retained-file reads are production.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (
    command: string,
    args: { bytes: number[]; expected: number },
  ) => {
    if (command !== "image_inflate_grain")
      throw Error("unexpected native command");
    const bytes = inflateSync(Uint8Array.from(args.bytes), {
      maxOutputLength: args.expected,
    });
    if (bytes.length !== args.expected) throw Error("short inflation");
    return Array.from(bytes);
  },
}));
const fixture = (name: string) =>
  Uint8Array.from(
    gunzipSync(
      readFileSync(new URL(`../tests/fixtures/${name}.gz`, import.meta.url)),
    ),
  );
const hash = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");
const options = { kind: "disk" as const, sector: 512 as const };
function source(files: [string, Uint8Array][]) {
  const group: NativeImageGroup = {
    id: "selection",
    files: files.map(([name, b], i) => ({
      id: `file-${i}`,
      name,
      size: b.length,
      lastModified: stamp,
    })),
  };
  const calls: number[] = [];
  const read: SourceRead = async (groupId, file, at, length) => {
    expect(groupId).toBe(group.id);
    const i = group.files.findIndex((f) => f.id === file);
    if (
      i < 0 ||
      length < 1 ||
      length > MiB ||
      at < 0 ||
      at > files[i][1].length - length
    )
      throw Error("invalid ordinary file read");
    calls.push(length);
    return files[i][1].slice(at, at + length);
  };
  return { group, read, calls };
}
it("uses immutable ordinary-file metadata, bounded native slices and exact replies", async () => {
  const bytes = new Uint8Array(3 * MiB + 7).map((_, i) => i % 251);
  const { group, read, calls } = source([["source.img", bytes]]);
  const file = retainedImageFile(group.id, group.files[0], read);
  group.files[0].name = "changed.img";
  expect(file.name).toBe("source.img");
  expect(Object.isFrozen(file)).toBe(true);
  expect(
    new Uint8Array(await file.slice(2, 2 * MiB + 3).arrayBuffer()),
  ).toEqual(bytes.slice(2, 2 * MiB + 3));
  expect(calls).toEqual([MiB, MiB, 1]);
  expect(new Uint8Array(await file.slice(-7).arrayBuffer())).toEqual(
    bytes.slice(-7),
  );
  expect((await file.slice(50, 10).arrayBuffer()).byteLength).toBe(0);
  const short = retainedImageFile(
    group.id,
    group.files[0],
    async () => new Uint8Array(1),
  );
  await expect(short.slice(0, 100).arrayBuffer()).rejects.toThrow("不完整");
  const big = retainedImageFile(
    group.id,
    { ...group.files[0], size: 128 * MiB },
    read,
  );
  expect(() => big.slice(0, 65 * MiB)).toThrow("64 MiB");
  for (const patch of [
    { name: "../x.img" },
    { size: Infinity },
    { lastModified: -1 },
  ])
    expect(() =>
      retainedImageFile(group.id, { ...group.files[0], ...patch }, read),
    ).toThrow();
});
it("reads random offsets and repeated passes across all supported container families", async () => {
  const normal = fixture("vhd-qemu-raw"),
    fourKn = fixture("vhdx-gpt4k-raw"),
    vmware = fixture("vmdk-raw");
  const cases: {
    files: [string, Uint8Array][];
    raw: Uint8Array;
    sector?: 512 | 4096;
  }[] = [
    { files: [["source.img", normal]], raw: normal },
    ...["fixed", "dynamic"].flatMap((kind) => [
      {
        files: [["disk.vhd", fixture(`vhd-qemu-${kind}`)]] as [
          string,
          Uint8Array,
        ][],
        raw: normal,
      },
      {
        files: [["disk.vdi", fixture(`vdi-qemu-${kind}`)]] as [
          string,
          Uint8Array,
        ][],
        raw: normal,
      },
      {
        files: [["disk.vhdx", fixture(`vhdx-qemu-${kind}`)]] as [
          string,
          Uint8Array,
        ][],
        raw: normal,
      },
    ]),
    {
      files: [["disk.vhdx", fixture("vhdx-qemu-gpt4k")]],
      raw: fourKn,
      sector: 4096,
    },
    ...["sparse", "zero", "stream"].map((kind) => ({
      files: [["disk.vmdk", fixture(`vmdk-qemu-${kind}`)]] as [
        string,
        Uint8Array,
      ][],
      raw: vmware,
    })),
    { files: [["disk.vmdk", fixture("vmdk-footer")]], raw: vmware },
    ...["extended", "legacy", "small"].map((kind) => ({
      files: [["disk.hds", fixture(`parallels-${kind}`)]] as [
        string,
        Uint8Array,
      ][],
      raw: vmware,
    })),
    {
      files: ["leaf.avhd", "middle.avhd", "base.vhd"].map((n) => [
        n,
        chainBytes(n),
      ]),
      raw: chainBytes("expected.raw"),
    },
    ...Object.values(bundleNames).map((names) => ({
      files: names.map((n) => [n, bundleBytes(n)] as [string, Uint8Array]),
      raw: vmware,
    })),
  ];
  for (const item of cases) {
    const before = item.files.map(([, b]) => hash(b));
    const s = source(item.files),
      image = await browseReader(s.group, options, s.read);
    expect(image.size).toBe(item.raw.length);
    expect(image.sectorSize).toBe(item.sector ?? 512);
    // More than inventory's 4MiB ceiling and more than one conversion pass,
    // including backwards and cross-block reads: browsing has a separate budget.
    for (const at of [
      image.size - MiB,
      0,
      MiB - 17,
      2 * MiB,
      0,
      image.size - MiB,
      MiB,
      0,
      MiB,
    ]) {
      const length = Math.min(MiB, image.size - at);
      expect(hash(await image.read(at, length))).toBe(
        hash(item.raw.subarray(at, at + length)),
      );
    }
    await image.verify();
    expect(item.files.map(([, b]) => hash(b))).toEqual(before);
    expect(Math.max(...s.calls)).toBeLessThanOrEqual(MiB);
  }
}, 60000);
it("rejects missing or ambiguous companions, invalid RAW geometry and unrelated extra files", async () => {
  for (const files of [
    [["leaf.avhd", chainBytes("leaf.avhd")]],
    [["split-flat.vmdk", bundleBytes("split-flat.vmdk")]],
    [["disk.img", new Uint8Array(MiB + 1)]],
    [
      ["disk.vdi", fixture("vdi-qemu-fixed")],
      ["extra.raw", new Uint8Array(MiB)],
    ],
    [
      ["disk.img", new Uint8Array(MiB)],
      ["DISK.IMG", new Uint8Array(MiB)],
    ],
  ] as [string, Uint8Array][][]) {
    const s = source(files);
    await expect(browseReader(s.group, options, s.read)).rejects.toThrow();
  }
});
const session = "11223344-5566-4788-99aa-bbccddeeff00",
  request = "01020304-0506-4708-890a-0b0c0d0e0f10";
const event = (reader = "r1"): ImageReadEvent => ({
  event: "read",
  session,
  request,
  reader,
  offset: 9,
  length: 5,
});
function logical(): LogicalImage {
  return {
    size: MiB,
    sectorSize: 512,
    format: "test",
    notes: [],
    warnings: [],
    verify: async () => {},
    read: async (at, n) => Uint8Array.from({ length: n }, (_, i) => at + i),
  };
}
it("frames opaque binary identities and rejects malformed or oversized responses", () => {
  const frame = imageReplyFrame(session, request, new Uint8Array([9, 8, 7]));
  expect(Buffer.from(frame.subarray(0, 16)).toString("hex")).toBe(
    session.replaceAll("-", ""),
  );
  expect(Buffer.from(frame.subarray(16, 32)).toString("hex")).toBe(
    request.replaceAll("-", ""),
  );
  expect([...frame.subarray(32)]).toEqual([9, 8, 7]);
  expect(() => imageReplyFrame("bad", request, new Uint8Array(1))).toThrow();
  expect(() =>
    imageReplyFrame(session, request, new Uint8Array(65537)),
  ).toThrow();
  expect(() => imageReplyFrame(session, request, new Uint8Array())).toThrow();
});
it("creates one decoder per operation and drops late responses after reader or session close", async () => {
  let resolve!: (image: LogicalImage) => void;
  const create = vi.fn(
      () =>
        new Promise<LogicalImage>((done) => {
          resolve = done;
        }),
    ),
    reply = vi.fn(async (_frame: Uint8Array) => {}),
    fail = vi.fn(async () => {});
  const server = new ImageReadServer(create, reply, fail);
  const pending = server.event(event());
  await server.event({ event: "readerClosed", session, reader: "r1" });
  resolve(logical());
  await pending;
  expect(reply).not.toHaveBeenCalled();
  expect(fail).not.toHaveBeenCalled();
  const next = server.event(event("r2"));
  resolve(logical());
  await next;
  await server.event(event("r2"));
  expect(create).toHaveBeenCalledTimes(2);
  expect(reply).toHaveBeenCalledTimes(2);
  expect([...reply.mock.calls[0][0].subarray(32)]).toEqual([9, 10, 11, 12, 13]);
  const last = server.event(event("r3"));
  await server.event({ event: "closed", session });
  resolve(logical());
  await last;
  await server.event(event("r4"));
  expect(create).toHaveBeenCalledTimes(3);
  expect(reply).toHaveBeenCalledTimes(2);
});
it("reports decoder failure once, limits active readers and never retries rejected replies", async () => {
  const fail = vi.fn(async () => {}),
    reply = vi.fn(async () => {
      throw Error("stale");
    }),
    create = vi.fn(async () => logical());
  const server = new ImageReadServer(create, reply, fail);
  for (let i = 0; i < 5; i++) await server.event(event(`r${i}`));
  expect(create).toHaveBeenCalledTimes(4);
  expect(reply).toHaveBeenCalledTimes(4);
  expect(fail).toHaveBeenCalledTimes(1);
  const bad = new ImageReadServer(
    async () => ({ ...logical(), read: async () => new Uint8Array(1) }),
    reply,
    fail,
  );
  await bad.event(event());
  expect(reply).toHaveBeenCalledTimes(4);
  expect(fail).toHaveBeenCalledTimes(2);
  const failed = new ImageReadServer(
    async () => {
      throw Error("damaged metadata");
    },
    reply,
    fail,
  );
  await failed.event(event());
  expect(fail).toHaveBeenCalledTimes(3);
});

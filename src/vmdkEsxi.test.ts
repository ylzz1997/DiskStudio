import { expect, it } from "vitest";
import { memoryFile } from "../tests/fixtures/vhd-memory";
import {
  COW_DIRECTORY,
  MiB,
  SE_DATA,
  SE_DIRECTORY,
  SE_TABLES,
  esxiDescriptor,
  makeCowd,
  makeSesparse,
} from "../tests/fixtures/esxi-vmdk";
import { openVmdkSource } from "./vmdkBundle";
import { parseVmdkDescriptor } from "./vmdkDescriptor";

const capacity = 6 * MiB;
const described = (name: string, descriptor: string) =>
  memoryFile(new TextEncoder().encode(descriptor), name).file;
function chain() {
  const raw = Buffer.alloc(capacity, 0x32);
  const cowd = makeCowd(capacity, [
    [4 * MiB + 3584, Buffer.alloc(512, 0x55)],
    [4 * MiB + 4096, Buffer.alloc(512, 0)],
    [0x40000, Buffer.alloc(512, 0x78)],
  ]);
  const sesparse = makeSesparse(capacity, [
    [3 * MiB, "unmapped"],
    [3 * MiB + 4096, "zero"],
    [5 * MiB, Buffer.alloc(4096, 0x95)],
  ]);
  const base = described(
    "base.vmdk",
    esxiDescriptor("vmfs", capacity, "base-flat.vmdk", "00000001"),
  );
  const middle = described(
    "point.vmdk",
    esxiDescriptor("vmfsSparse", capacity, "point-delta.vmdk", "00000002", {
      name: "base.vmdk",
      cid: "00000001",
    }),
  );
  const leaf = described(
    "latest.vmdk",
    esxiDescriptor(
      "seSparse",
      capacity,
      "latest-sesparse.vmdk",
      "00000003",
      { name: "point.vmdk", cid: "00000002" },
      3,
    ) + 'changeTrackPath="../latest-ctk.vmdk"\nddb.grain="8"\n',
  );
  const companions = [
    memoryFile(raw, "base-flat.vmdk").file,
    middle,
    memoryFile(sesparse, "latest-sesparse.vmdk").file,
    base,
    memoryFile(cowd, "point-delta.vmdk").file,
  ];
  return { leaf, companions, raw, cowd, sesparse };
}

it("reads a mixed ESXi chain with VMFS, COWD and SEsparse extents", async () => {
  const { leaf, companions, raw, sesparse } = chain();
  const unused = memoryFile(new Uint8Array(512), "latest-ctk.vmdk");
  const image = await openVmdkSource(
    leaf,
    [...companions, unused.file],
    undefined,
    "browse",
  );
  expect(image.chain).toEqual(["latest.vmdk", "point.vmdk", "base.vmdk"]);
  expect(image.fileCount).toBe(6);
  expect(image.sectorSize).toBe(512);
  const expected = Buffer.from(raw);
  expected.fill(0x78, 0x40000, 0x40200);
  expected.fill(0, 3 * MiB, 3 * MiB + 8192);
  expected.fill(0x55, 4 * MiB + 3584, 4 * MiB + 4096);
  expected.fill(0, 4 * MiB + 4096, 4 * MiB + 4608);
  expected.fill(0x95, 5 * MiB, 5 * MiB + 4096);
  for (let at = 0; at < expected.length;) {
    const count = Math.min(187751, expected.length - at);
    expect(
      Buffer.from(await image.read(at, count)).equals(
        expected.subarray(at, at + count),
      ),
    ).toBe(true);
    at += count;
  }
  await image.verify();
  expect(unused.reads).toEqual([]);
  sesparse[SE_TABLES] ^= 1;
  await expect(image.verify()).rejects.toThrow("发生变化");
});

it("uses the descriptor for parent and extent selection and checks the header type", async () => {
  const { leaf, companions } = chain();
  await expect(openVmdkSource(leaf)).rejects.toMatchObject({
    kind: "parent",
    missing: ["point.vmdk"],
  });
  await expect(
    openVmdkSource(
      leaf,
      companions.filter(({ name }) => name !== "point-delta.vmdk"),
    ),
  ).rejects.toMatchObject({ missing: ["point-delta.vmdk"] });
  await expect(openVmdkSource(companions[2])).rejects.toThrow("主描述文件");
  const wrong = described(
    "base.vmdk",
    esxiDescriptor("vmfs", capacity, "base-flat.vmdk", "00000009"),
  );
  await expect(
    openVmdkSource(leaf, [
      ...companions.filter(({ name }) => name !== "base.vmdk"),
      wrong,
    ]),
  ).rejects.toThrow("父盘 CID");
  for (const [type, extent] of [
    ["vmfs", "flat.vmdk"],
    ["vmfsSparse", "delta.vmdk"],
    ["seSparse", "data.vmdk"],
  ] as const) {
    const valid = esxiDescriptor(type, capacity, extent, "00000001");
    expect(parseVmdkDescriptor(Buffer.from(valid)).extents[0].offset).toBe(0);
    expect(() =>
      parseVmdkDescriptor(
        Buffer.from(valid.replace(`"${extent}"`, `"${extent}" 1`)),
      ),
    ).toThrow("偏移");
  }
});

it("reads SEsparse slot numbers that use both packed index fields and respects the grain area bounds", async () => {
  const size = 32 * MiB;
  const image = makeSesparse(
    size,
    [[18 * MiB, Buffer.alloc(4096, 0x6d)]],
    4097,
  );
  // Newer VMware writes the const header size in this reserved field.
  image.writeBigUInt64LE(512n, 48);
  const desc = described(
    "only.vmdk",
    esxiDescriptor("seSparse", size, "stored.vmdk", "00000001"),
  );
  const open = () =>
    openVmdkSource(desc, [memoryFile(image, "stored.vmdk").file]);
  const source = await open();
  expect(await source.read(18 * MiB + 193, 501)).toEqual(
    new Uint8Array(501).fill(0x6d),
  );
  image.writeBigUInt64LE(4096n, 200);
  await expect(open()).rejects.toThrow("已存储区段");
});

it("rejects dirty ESXi snapshots and invalid or overlapping metadata/data mappings", async () => {
  for (const [kind, bad, message] of [
    ["cowd", (b: Buffer) => b.writeUInt32LE(1, 1648), "未正常关闭"],
    ["cowd", (b: Buffer) => b.writeUInt32LE(4, COW_DIRECTORY), "重叠"],
    [
      "cowd",
      (b: Buffer) =>
        b.writeUInt32LE(
          4,
          b.readUInt32LE(COW_DIRECTORY) * 512 + (0x40000 / 512) * 4,
        ),
      "数据粒度与元数据重叠",
    ],
    ["cowd", (b: Buffer) => b.writeUInt32LE(11, 12), "容量"],
    ["se", (b: Buffer) => b.writeBigUInt64LE(1n, 536), "待回放日志"],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE(2n << 60n, SE_DIRECTORY),
      "标志无效",
    ],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE((1n << 60n) | 32n, SE_DIRECTORY),
      "越界",
    ],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE(2n << 60n, SE_DIRECTORY + 8),
      "逻辑容量之外",
    ],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE(BigInt(SE_DATA / 512 - 1), 192),
      "区段重叠",
    ],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE(4n << 60n, SE_TABLES),
      "分配状态无效",
    ],
    [
      "se",
      (b: Buffer) => b.writeBigUInt64LE(3n << 60n, SE_TABLES),
      "重复或重叠",
    ],
  ] as const) {
    const { leaf, companions, cowd, sesparse } = chain();
    bad(kind === "cowd" ? cowd : sesparse);
    await expect(openVmdkSource(leaf, companions)).rejects.toThrow(message);
  }
});

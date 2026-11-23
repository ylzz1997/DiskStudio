export const MiB = 1024 ** 2;
export const COW_DIRECTORY = 4 * 512;
export const SE_DIRECTORY = 4096 * 512;
export const SE_TABLES = 6144 * 512;
export const SE_DATA = 12288 * 512;
const TABLE_ENTRIES = 4096;

export function esxiDescriptor(
  type: "vmfs" | "vmfsSparse" | "seSparse",
  bytes: number,
  extent: string,
  cid: string,
  parent?: { name: string; cid: string },
  version = 1,
) {
  const kind = { vmfs: "VMFS", vmfsSparse: "VMFSSPARSE", seSparse: "SESPARSE" }[
    type
  ];
  const ancestry = parent ? `parentFileNameHint="${parent.name}"\n` : "";
  return `# Disk DescriptorFile\nversion=${version}\nencoding="UTF-8"\nCID=${cid}\nparentCID=${parent?.cid ?? "ffffffff"}\ncreateType="${type}"\n${ancestry}# Extent description\nRW ${bytes / 512} ${kind} "${extent}"\n`;
}

export function makeCowd(
  size: number,
  records: [number, Uint8Array][],
  grain = 512,
) {
  const count = Math.ceil(size / (grain * TABLE_ENTRIES));
  const tableStart = COW_DIRECTORY + Math.ceil((count * 4) / 512) * 512;
  const tableIds = [
    ...new Set(records.map(([at]) => Math.floor(at / (grain * TABLE_ENTRIES)))),
  ];
  const payloadStart = tableStart + tableIds.length * 16 * 1024;
  const b = Buffer.alloc(payloadStart + records.length * grain);
  b.write("COWD");
  b.writeUInt32LE(1, 4);
  b.writeUInt32LE(3, 8);
  b.writeUInt32LE(size / 512, 12);
  b.writeUInt32LE(grain / 512, 16);
  b.writeUInt32LE(COW_DIRECTORY / 512, 20);
  b.writeUInt32LE(count, 24);
  b.writeUInt32LE(b.length / 512, 28);
  tableIds.forEach((id, i) =>
    b.writeUInt32LE((tableStart + i * 16 * 1024) / 512, COW_DIRECTORY + id * 4),
  );
  records.forEach(([at, content], i) => {
    if (at % grain || at >= size || content.length !== grain)
      throw Error("Invalid COWD fixture grain");
    const logical = at / grain;
    const table = tableIds.indexOf(Math.floor(logical / TABLE_ENTRIES));
    const entry =
      tableStart + table * 16 * 1024 + (logical % TABLE_ENTRIES) * 4;
    const physical = payloadStart + i * grain;
    b.writeUInt32LE(physical / 512, entry);
    b.set(content, physical);
  });
  return b;
}

export function makeSesparse(
  size: number,
  records: [number, Uint8Array | "zero" | "unmapped"][],
  firstSlot = 0,
) {
  const grain = 4096;
  const tableIds = [
    ...new Set(records.map(([at]) => Math.floor(at / (grain * TABLE_ENTRIES)))),
  ];
  if (size > 512 * MiB)
    throw Error("Fixture exceeds the fixed one-MiB table region");
  const stored = records.filter(
    ([, value]) => typeof value !== "string",
  ).length;
  const b = Buffer.alloc(SE_DATA + (firstSlot + stored) * grain);
  const fields = [
    0xcafebaben,
    0x0000000200000001n,
    size / 512,
    8,
    64,
    0,
    0,
    0,
    0,
    0,
    1,
    1,
    2,
    2,
    2048,
    2048,
    SE_DIRECTORY / 512,
    2048,
    SE_TABLES / 512,
    2048,
    8192,
    2048,
    10240,
    2048,
    SE_DATA / 512,
    size / 512,
  ];
  fields.forEach((value, i) => b.writeBigUInt64LE(BigInt(value), i * 8));
  b.writeBigUInt64LE(0xcafecafen, 512);
  tableIds.forEach((logical, slot) =>
    b.writeBigUInt64LE((1n << 60n) | BigInt(slot), SE_DIRECTORY + logical * 8),
  );
  let slot = firstSlot;
  for (const [at, content] of records) {
    if (at % grain || at >= size) throw Error("Invalid SESPARSE fixture grain");
    const logical = at / grain;
    const table = tableIds.indexOf(Math.floor(logical / TABLE_ENTRIES));
    const entry =
      SE_TABLES + table * TABLE_ENTRIES * 8 + (logical % TABLE_ENTRIES) * 8;
    if (content === "zero") b.writeBigUInt64LE(2n << 60n, entry);
    else if (content === "unmapped")
      b.writeBigUInt64LE((1n << 60n) | (7n << 48n), entry);
    else {
      if (content.length !== grain)
        throw Error("Wrong SESPARSE fixture grain length");
      b.writeBigUInt64LE(
        (3n << 60n) |
          (BigInt(slot % 4096) << 48n) |
          BigInt(Math.floor(slot / 4096)),
        entry,
      );
      b.set(content, SE_DATA + slot * grain);
      slot++;
    }
  }
  return b;
}

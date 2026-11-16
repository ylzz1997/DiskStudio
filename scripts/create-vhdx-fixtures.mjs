// Ordinary synthetic files only. Offline qemu-img, never a running VM/device.
import { mkdir, readFile, writeFile, lstat } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { gzipSync, gunzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url),
  output = new URL(".vm/vhdx-fixtures/", root),
  fixtures = new URL("tests/fixtures/", root);
await mkdir(output, { recursive: true });
for (const dir of [new URL(".vm/", root), output, fixtures])
  if ((await lstat(dir)).isSymbolicLink())
    throw Error("Fixture directories must not be links");
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
console.log(
  execFileSync(tool, ["--version"], {
    encoding: "utf8",
    windowsHide: true,
  }).split("\n")[0],
);
const mbr = gunzipSync(await readFile(new URL("vhd-qemu-raw.gz", fixtures)));
const MiB = 1024 ** 2,
  gpt = Buffer.alloc(8 * MiB),
  sector = 4096,
  last = gpt.length / sector - 1;
const crc32 = (bytes) => {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++)
      crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  return ~crc >>> 0;
};
gpt[510] = 0x55;
gpt[511] = 0xaa;
gpt[450] = 0xee;
gpt.writeUInt32LE(1, 454);
gpt.writeUInt32LE(last, 458);
const table = Buffer.alloc(16384);
table.fill(1, 0, 16);
table.fill(2, 16, 32);
table.writeBigUInt64LE(256n, 32);
table.writeBigUInt64LE(1023n, 40);
table.write("VHDX 数据", 56, "utf16le");
for (const [own, other, at] of [
  [1, last, 2],
  [last, 1, last - 4],
]) {
  table.copy(gpt, at * sector);
  const h = gpt.subarray(own * sector, (own + 1) * sector);
  h.write("EFI PART");
  h.writeUInt32LE(0x10000, 8);
  h.writeUInt32LE(92, 12);
  h.writeBigUInt64LE(BigInt(own), 24);
  h.writeBigUInt64LE(BigInt(other), 32);
  h.writeBigUInt64LE(6n, 40);
  h.writeBigUInt64LE(BigInt(last - 5), 48);
  h.fill(3, 56, 72);
  h.writeBigUInt64LE(BigInt(at), 72);
  h.writeUInt32LE(128, 80);
  h.writeUInt32LE(128, 84);
  h.writeUInt32LE(crc32(table), 88);
  h.writeUInt32LE(crc32(h.subarray(0, 92)), 16);
}
for (const [name, raw, format] of [
  ["fixed", mbr, "fixed"],
  ["dynamic", mbr, "dynamic"],
  ["gpt4k", gpt, "dynamic"],
]) {
  const source = new URL(`synthetic-${name}.raw`, output),
    dest = new URL(`synthetic-${name}.vhdx`, output);
  await writeFile(source, raw, { flag: "wx" });
  try {
    await lstat(dest);
    throw Error("Fixture exists");
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  execFileSync(
    tool,
    [
      "convert",
      "-f",
      "raw",
      "-O",
      "vhdx",
      "-o",
      `subformat=${format},block_size=1048576`,
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  let bytes = await readFile(dest);
  if (name === "gpt4k") {
    // QEMU's writer exposes no logical-sector option. Change only the two known
    // sector-size metadata values in this disposable output. QEMU's reader also
    // rejects 4Kn, so 7-Zip independently extracts/compares this variant below.
    const regionGuid = Buffer.from("06a27c8b90479a4bb8fe575f050f886e", "hex");
    let metaAt;
    for (let i = 0; i < bytes.readUInt32LE(192 * 1024 + 8); i++) {
      const p = 192 * 1024 + 16 + i * 32;
      if (bytes.subarray(p, p + 16).equals(regionGuid))
        metaAt = Number(bytes.readBigUInt64LE(p + 16));
    }
    if (metaAt === undefined) throw Error("Missing metadata");
    for (let i = 0; i < bytes.readUInt16LE(metaAt + 10); i++) {
      const p = metaAt + 32 + i * 32,
        id = bytes.subarray(p, p + 16).toString("hex");
      if (
        [
          "1dbf41816fa90947ba47f233a8faab5f",
          "c748a3cd5d4471449cc9e9885251c556",
        ].includes(id)
      )
        bytes.writeUInt32LE(4096, metaAt + bytes.readUInt32LE(p + 16));
    }
    await writeFile(dest, bytes);
  }
  if (name !== "gpt4k") {
    execFileSync(tool, ["check", "-f", "vhdx", fileURLToPath(dest)], {
      windowsHide: true,
    });
    execFileSync(
      tool,
      [
        "compare",
        "-f",
        "raw",
        "-F",
        "vhdx",
        fileURLToPath(source),
        fileURLToPath(dest),
      ],
      { windowsHide: true },
    );
  } else {
    const seven = fileURLToPath(new URL(".vm/tools/7zip/7z.exe", root));
    const extracted = execFileSync(
      seven,
      ["x", "-so", "-tVHDX", fileURLToPath(dest)],
      { windowsHide: true, maxBuffer: 16 * MiB },
    );
    if (!raw.equals(extracted)) throw Error("Independent 7-Zip bytes differ");
  }
  await writeFile(new URL(`vhdx-qemu-${name}.gz`, fixtures), gzipSync(bytes), {
    flag: "wx",
  });
  if (name === "gpt4k")
    await writeFile(new URL("vhdx-gpt4k-raw.gz", fixtures), gzipSync(raw), {
      flag: "wx",
    });
  console.log(`${name}: ${bytes.length} B; independent byte comparison passed`);
}

// QEMU's offline image converter creates independent fixtures from synthetic
// project data. This does not start/contact a VM, attach storage or open devices.
import { mkdir, readFile, writeFile, lstat } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { gzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url);
const output = new URL(".vm/vhd-fixtures/", root);
const fixtures = new URL("tests/fixtures/", root);
await mkdir(output, { recursive: true });
for (const dir of [new URL(".vm/", root), output, fixtures])
  if ((await lstat(dir)).isSymbolicLink())
    throw new Error("Fixture directories must not be links");
const raw = Buffer.alloc(8 * 1024 ** 2);
function entry(at, slot, type, start, count) {
  raw[at + 510] = 0x55;
  raw[at + 511] = 0xaa;
  const e = at + 446 + slot * 16;
  raw[e + 4] = type;
  raw.writeUInt32LE(start, e + 8);
  raw.writeUInt32LE(count, e + 12);
}
entry(0, 0, 0x0f, 2048, 12288);
entry(1024 ** 2, 0, 0x83, 2048, 2048);
entry(1024 ** 2, 1, 0x0f, 6144, 6144);
entry(4 * 1024 ** 2, 0, 7, 2048, 2048);
const source = new URL("synthetic-extended.raw", output);
await writeFile(source, raw, { flag: "wx" });
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
console.log(
  execFileSync(tool, ["--version"], {
    encoding: "utf8",
    windowsHide: true,
  }).split("\n")[0],
);
for (const format of ["fixed", "dynamic"]) {
  const dest = new URL(`synthetic-${format}.vhd`, output);
  try {
    await lstat(dest);
    throw new Error("Fixture already exists");
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
      "vpc",
      "-o",
      `subformat=${format},force_size=on`,
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  const bytes = await readFile(dest);
  await writeFile(new URL(`vhd-qemu-${format}.gz`, fixtures), gzipSync(bytes), {
    flag: "wx",
  });
  console.log(`${format}: ${bytes.length} bytes`);
}
await writeFile(new URL("vhd-qemu-raw.gz", fixtures), gzipSync(raw), {
  flag: "wx",
});

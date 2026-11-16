// Creates only new synthetic ordinary files under this project; no VM/device I/O.
import { mkdir, lstat, readFile, writeFile, readdir } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";
const root = new URL("../", import.meta.url),
  output = new URL(".vm/vmdk-bundle-fixtures/", root),
  fixtures = new URL("tests/fixtures/", root);
for (const dir of [new URL(".vm/", root), fixtures])
  if ((await lstat(dir)).isSymbolicLink())
    throw Error("Fixture directories must not be links");
await mkdir(output); // Exclusive new directory: never overwrite existing fixtures.
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
const run = (args) =>
  console.log(
    execFileSync(tool, args, { windowsHide: true, encoding: "utf8" }).trim(),
  );
const path = (name) => fileURLToPath(new URL(name, output));
const write = (name, data) =>
  writeFile(new URL(name, output), data, { flag: "wx" });
const raw = gunzipSync(await readFile(new URL("vmdk-raw.gz", fixtures)));
await write("synthetic.raw", raw);
run(["--version"]);
for (const [name, type] of [
  ["flat", "monolithicFlat"],
  ["qemu-split", "twoGbMaxExtentSparse"],
])
  run([
    "convert",
    "-f",
    "raw",
    "-O",
    "vmdk",
    "-o",
    `subformat=${type}`,
    path("synthetic.raw"),
    path(`${name}.vmdk`),
  ]);
// Small extents exercise real cross-file reads without generating a >2 GiB file.
// Each sparse extent is independently created by QEMU, then referenced in order.
for (let i = 0; i < 2; i++) {
  await write(
    `part${i}.raw`,
    raw.subarray((i * raw.length) / 2, ((i + 1) * raw.length) / 2),
  );
  run([
    "convert",
    "-f",
    "raw",
    "-O",
    "vmdk",
    "-o",
    "subformat=twoGbMaxExtentSparse",
    path(`part${i}.raw`),
    path(`part${i}.vmdk`),
  ]);
}
const desc = (type, lines) =>
  `# Disk DescriptorFile\nversion=1\nCID=12345678\nparentCID=ffffffff\ncreateType="${type}"\n${lines.join("\n")}\n`;
await write(
  "split-sparse.vmdk",
  desc(
    "twoGbMaxExtentSparse",
    [0, 1].map((i) => `RW 8192 SPARSE "part${i}-s001.vmdk"`),
  ),
);
// Nonzero 512-byte prefixes ensure FLAT offsets are applied.
for (let i = 0; i < 2; i++)
  await write(
    `flat${i}.bin`,
    Buffer.concat([
      Buffer.alloc(512, 0xa5),
      raw.subarray((i * raw.length) / 2, ((i + 1) * raw.length) / 2),
    ]),
  );
await write(
  "split-flat.vmdk",
  desc(
    "twoGbMaxExtentFlat",
    [0, 1].map((i) => `RDONLY 8192 FLAT "flat${i}.bin" 1`),
  ),
);
// QEMU supports RW descriptors only; use identical range metadata with RW for its oracle.
await write(
  "split-flat-oracle.vmdk",
  desc(
    "twoGbMaxExtentFlat",
    [0, 1].map((i) => `RW 8192 FLAT "flat${i}.bin" 1`),
  ),
);
for (const name of [
  "flat",
  "qemu-split",
  "split-sparse",
  "split-flat-oracle",
]) {
  run(["check", "-f", "vmdk", path(`${name}.vmdk`)]);
  run([
    "compare",
    "-f",
    "raw",
    "-F",
    "vmdk",
    path("synthetic.raw"),
    path(`${name}.vmdk`),
  ]);
}
for (const name of await readdir(output)) {
  if (
    name.endsWith(".raw") ||
    name.endsWith("-oracle.vmdk") ||
    /^part\d\.vmdk$/.test(name)
  )
    continue;
  const bytes = await readFile(new URL(name, output));
  await writeFile(
    new URL(`vmdk-bundle-${name}.gz`, fixtures),
    gzipSync(bytes),
    { flag: "wx" },
  );
  console.log(JSON.stringify({ name, size: bytes.length }));
}

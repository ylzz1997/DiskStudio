// Offline synthetic-file generation only. No VM, devices, mounts or repair.
import { mkdir, readFile, writeFile, lstat } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { gzipSync, gunzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url),
  output = new URL(".vm/parallels-fixtures/", root),
  fixtures = new URL("tests/fixtures/", root);
await mkdir(output, { recursive: true });
for (const dir of [new URL(".vm/", root), output, fixtures])
  if ((await lstat(dir)).isSymbolicLink())
    throw Error("Fixture directory is a link");
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
const raw = gunzipSync(await readFile(new URL("vmdk-raw.gz", fixtures)));
const source = new URL("synthetic-extended.raw", output);
const resume = process.argv.includes("--resume");
async function existing(file) {
  try {
    const stat = await lstat(file);
    if (!resume || !stat.isFile() || stat.isSymbolicLink() || stat.nlink !== 1)
      throw Error(
        "Existing output requires --resume and an ordinary single-link file",
      );
    return true;
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
    return false;
  }
}
async function writeExact(file, bytes) {
  if (await existing(file)) {
    if (!(await readFile(file)).equals(bytes))
      throw Error("Existing fixture differs");
  } else await writeFile(file, bytes, { flag: "wx" });
}
await writeExact(source, raw);
console.log(
  execFileSync(tool, ["--version"], {
    encoding: "utf8",
    windowsHide: true,
  }).split("\n")[0],
);
async function check(kind, bytes) {
  const dest = new URL(`synthetic-${kind}.hdd`, output);
  if (bytes) await writeExact(dest, bytes);
  // Windows QEMU 10.2 and 11.1 check both crash with 0xc0000374 on this
  // valid synthetic file. Do not claim structural check acceptance: use its
  // full logical read/compare here, plus product corruption tests separately.
  for (const args of [
    [
      "compare",
      "-f",
      "raw",
      "-F",
      "parallels",
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
  ])
    console.log(
      execFileSync(tool, args, { encoding: "utf8", windowsHide: true }).trim(),
    );
  const b = await readFile(dest);
  await writeExact(new URL(`parallels-${kind}.gz`, fixtures), gzipSync(b));
  console.log(
    JSON.stringify({ kind, length: b.length, header: [...b.subarray(0, 64)] }),
  );
  return b;
}
for (const [kind, size] of [
  ["extended", 1048576],
  ["small", 65536],
]) {
  const dest = new URL(`synthetic-${kind}.hdd`, output);
  if (!(await existing(dest)))
    execFileSync(
      tool,
      [
        "convert",
        "-f",
        "raw",
        "-O",
        "parallels",
        "-o",
        `cluster_size=${size}`,
        fileURLToPath(source),
        fileURLToPath(dest),
      ],
      { windowsHide: true },
    );
  const b = await check(kind);
  if (kind === "extended") {
    // The old signature stores BAT addresses in sectors, not clusters. Pack
    // unchanged independent data clusters after a sector-aligned BAT/header.
    const count = b.readUInt32LE(32),
      track = b.readUInt32LE(28),
      cluster = track * 512;
    const dataStart = Math.ceil((64 + count * 4) / 512) * 512;
    const allocated = Array.from({ length: count }, (_, i) => [
      i,
      b.readUInt32LE(64 + i * 4),
    ])
      .filter(([, n]) => n !== 0)
      .reverse();
    const old = Buffer.alloc(dataStart + allocated.length * cluster);
    b.copy(old, 0, 0, 64);
    old.write("WithoutFreeSpace", 0, "ascii");
    old.writeUInt32LE(0, 48);
    allocated.forEach(([index, slot], n) => {
      const at = dataStart + n * cluster;
      old.writeUInt32LE(at / 512, 64 + index * 4);
      b.copy(old, at, slot * cluster, (slot + 1) * cluster);
    });
    await check("legacy", old);
  }
}

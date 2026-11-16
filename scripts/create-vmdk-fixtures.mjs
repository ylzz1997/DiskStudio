// Offline conversion of synthetic project files only; never devices or VMs.
import { mkdir, readFile, writeFile, lstat } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { gzipSync, gunzipSync, inflateSync } from "node:zlib";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url),
  output = new URL(".vm/vmdk-fixtures/", root),
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
const raw = gunzipSync(await readFile(new URL("vhd-qemu-raw.gz", fixtures)));
// Nonzero data at both ends forces real compressed reads through the UI.
for (const at of [8192, raw.length - 65536])
  for (let i = 0; i < 65536; i++)
    raw[at + i] = (i * 31 + Math.floor(i / 257)) % 251;
const source = new URL("synthetic-extended.raw", output);
await writeFile(source, raw, { flag: "wx" });
await writeFile(new URL("vmdk-raw.gz", fixtures), gzipSync(raw), {
  flag: "wx",
});
for (const kind of ["sparse", "zero", "stream"]) {
  const dest = new URL(`synthetic-${kind}.vmdk`, output);
  try {
    await lstat(dest);
    throw Error("Fixture already exists");
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  const opts =
    kind === "stream"
      ? "subformat=streamOptimized"
      : `subformat=monolithicSparse,zeroed_grain=${kind === "zero" ? "on" : "off"}`;
  execFileSync(
    tool,
    [
      "convert",
      ...(kind === "stream" ? ["-c"] : []),
      "-f",
      "raw",
      "-O",
      "vmdk",
      "-o",
      opts,
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  execFileSync(tool, ["check", "-f", "vmdk", fileURLToPath(dest)], {
    windowsHide: true,
  });
  execFileSync(
    tool,
    [
      "compare",
      "-f",
      "raw",
      "-F",
      "vmdk",
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  const bytes = await readFile(dest);
  await writeFile(new URL(`vmdk-qemu-${kind}.gz`, fixtures), gzipSync(bytes), {
    flag: "wx",
  });
  console.log(
    JSON.stringify({
      kind,
      length: bytes.length,
      version: bytes.readUInt32LE(4),
      flags: bytes.readUInt32LE(8),
      gd: Number(bytes.readBigUInt64LE(56)),
      rgd: Number(bytes.readBigUInt64LE(48)),
      overhead: Number(bytes.readBigUInt64LE(64)),
      descriptor: bytes.subarray(512, 1300).toString().split("\0")[0],
    }),
  );
  if (kind === "stream") {
    const gd = Number(bytes.readBigUInt64LE(56)) * 512,
      gt = bytes.readUInt32LE(gd) * 512,
      grain = bytes.readUInt32LE(gt) * 512;
    const size = bytes.readUInt32LE(grain + 8),
      compressed = bytes.subarray(grain + 12, grain + 12 + size);
    if (!inflateSync(compressed).equals(raw.subarray(0, 65536)))
      throw Error("First grain differs");
    await writeFile(new URL("vmdk-zlib-grain.bin", fixtures), compressed, {
      flag: "wx",
    });
  }
}

// Repackage the independent QEMU compressed grains into the documented
// footer-index layout. Only new synthetic project files are written.
import { readFile, writeFile, lstat } from "node:fs/promises";
import { gunzipSync, gzipSync } from "node:zlib";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url);
for (const dir of [".vm/", ".vm/vmdk-fixtures/", "tests/fixtures/"])
  if ((await lstat(new URL(dir, root))).isSymbolicLink())
    throw Error("Linked fixture directory");
const source = gunzipSync(
  await readFile(new URL("tests/fixtures/vmdk-qemu-stream.gz", root)),
);
const oldGd = Number(source.readBigUInt64LE(56)) * 512;
const oldGt = source.readUInt32LE(oldGd) * 512;
const gt = source.length + 512,
  gd = gt + 2048 + 512,
  footer = gd + 512 + 512;
const b = Buffer.alloc(footer + 1024);
source.copy(b);
b.fill(0, 21 * 512, 128 * 512);
b.writeUInt32LE(0x30001, 8);
b.writeBigUInt64LE(0n, 48);
b.writeBigUInt64LE(0xffffffffffffffffn, 56);
source.copy(b, gt, oldGt, oldGt + 2048);
b.writeUInt32LE(gt / 512, gd);
for (const [at, sectors, type] of [
  [gt - 512, 4, 1],
  [gd - 512, 1, 2],
  [footer - 512, 1, 3],
]) {
  b.writeBigUInt64LE(BigInt(sectors), at);
  b.writeUInt32LE(type, at + 12);
}
b.copy(b, footer, 0, 512);
b.writeBigUInt64LE(BigInt(gd / 512), footer + 56);
const dest = new URL(".vm/vmdk-fixtures/synthetic-footer.vmdk", root);
await writeFile(dest, b, { flag: "wx" });
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
for (const args of [
  ["check", "-f", "vmdk", fileURLToPath(dest)],
  [
    "compare",
    "-f",
    "raw",
    "-F",
    "vmdk",
    fileURLToPath(new URL(".vm/vmdk-fixtures/synthetic-extended.raw", root)),
    fileURLToPath(dest),
  ],
])
  console.log(
    execFileSync(tool, args, { encoding: "utf8", windowsHide: true }).trim(),
  );
await writeFile(new URL("tests/fixtures/vmdk-footer.gz", root), gzipSync(b), {
  flag: "wx",
});
console.log(JSON.stringify({ length: b.length, gt, gd, footer }));

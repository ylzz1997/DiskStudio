// Ordinary synthetic images only. QEMU generates the base. The two
// differencing files use Oracle VDI 1.1 header and block definitions.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { root, safePath } from "./lab.mjs";

const MiB = 1024 ** 2;
const source = safePath(path.join(root, "payload/ImageBrowse/disk-gpt512.img"));
const output = safePath(path.join(root, "vdi-chain-fixture"), root, false);
if (!fs.lstatSync(source).isFile() || fs.statSync(source).size !== 6 * MiB) {
  throw Error(
    "Run scripts/prepare-image-browse.mjs to generate the synthetic NTFS disk first.",
  );
}
fs.mkdirSync(output);
const write = (name, data) =>
  fs.writeFileSync(path.join(output, name), data, { flag: "wx" });
const read = (name) => fs.readFileSync(safePath(path.join(output, name)));
const raw = fs.readFileSync(source);
raw.fill(0x42, 3 * MiB, 4 * MiB);
write("base.raw", raw);
const qemu = safePath(path.join(root, "tools/qemu-10.2/qemu-img.exe"));
for (const args of [
  ["convert", "-f", "raw", "-O", "vdi", "base.raw", "base.vdi"],
  ["check", "-f", "vdi", "base.vdi"],
  ["compare", "-f", "vdi", "-F", "raw", "base.vdi", "base.raw"],
])
  console.log(
    execFileSync(qemu, args, {
      cwd: output,
      encoding: "utf8",
      windowsHide: true,
    }).trim(),
  );

const base = read("base.vdi");
const mapAt = base.readUInt32LE(340),
  dataAt = base.readUInt32LE(344);
if (
  base.readUInt32LE(68) !== 0x10001 ||
  base.readUInt32LE(376) !== MiB ||
  base.readUInt32LE(384) !== 6 ||
  base.readBigUInt64LE(368) !== BigInt(raw.length)
) {
  throw Error("Unexpected QEMU base VDI layout");
}
function child(parent, id, storedBlock, data, zeroBlock) {
  const out = Buffer.alloc(dataAt + MiB);
  base.copy(out, 0, 0, dataAt);
  out.writeUInt32LE(4, 76);
  out.writeUInt32LE(1, 388);
  out.fill(id, 392, 408);
  out.fill(id + 1, 408, 424);
  parent.copy(out, 424, 392, 424);
  out.fill(0xff, mapAt, mapAt + 24);
  out.writeUInt32LE(0, mapAt + storedBlock * 4);
  if (zeroBlock !== undefined)
    out.writeUInt32LE(0xfffffffe, mapAt + zeroBlock * 4);
  data.copy(out, dataAt);
  return out;
}
const middle = child(base, 0x68, 4, Buffer.alloc(MiB, 0x63));
const leading = Buffer.from(raw.subarray(0, MiB));
leading.fill(0x74, 0x40000, 0x50000);
const leaf = child(middle, 0x72, 0, leading, 3);
write("middle.vdi", middle);
write("latest.vdi", leaf);

// The oracle modifies the original logical bytes directly. It never reads
// the child block tables or imports DiskStudio's parser.
raw.fill(0x74, 0x40000, 0x50000);
raw.fill(0, 3 * MiB, 4 * MiB);
raw.fill(0x63, 4 * MiB, 5 * MiB);
write("expected.img", raw);
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const manifest = {
  files: {
    "base.vdi": sha(base),
    "middle.vdi": sha(middle),
    "latest.vdi": sha(leaf),
  },
  logicalSha256: sha(raw),
};
write("vdi-chain.json", JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));

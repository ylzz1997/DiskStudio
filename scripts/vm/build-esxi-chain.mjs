// Exercise an ordinary VMFS base and both ESXi delta layouts. QEMU reads
// the complete synthetic image chain independently before guest acceptance.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  esxiDescriptor,
  makeCowd,
  makeSesparse,
  MiB,
} from "../../tests/fixtures/esxi-vmdk.ts";
import { root, safePath } from "./lab.mjs";

const source = safePath(path.join(root, "payload/ImageBrowse/disk-gpt512.img"));
const output = safePath(path.join(root, "esxi-chain-fixture"), root, false);
const qemu = safePath(path.join(root, "tools/qemu-10.2/qemu-img.exe"));
const base = fs.readFileSync(source);
if (base.length !== 6 * MiB)
  throw Error("Expected the 6 MiB synthetic GPT/NTFS fixture");
fs.mkdirSync(output);
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const files = {};
function write(name, bytes) {
  fs.writeFileSync(path.join(output, name), bytes, { flag: "wx" });
  if (name.endsWith(".vmdk")) files[name] = sha(bytes);
}
const descriptors = [
  [
    "base.vmdk",
    esxiDescriptor("vmfs", base.length, "base-flat.vmdk", "b9876543"),
  ],
  [
    "point.vmdk",
    esxiDescriptor("vmfsSparse", base.length, "point-delta.vmdk", "a1234567", {
      name: "base.vmdk",
      cid: "b9876543",
    }),
  ],
  [
    "latest.vmdk",
    esxiDescriptor(
      "seSparse",
      base.length,
      "latest-sesparse.vmdk",
      "f6543210",
      {
        name: "point.vmdk",
        cid: "a1234567",
      },
    ),
  ],
];
for (const [name, descriptor] of descriptors) write(name, descriptor);
base.fill(0x42, 3 * MiB, 3 * MiB + 8192);
write("base-flat.vmdk", base);
write(
  "point-delta.vmdk",
  makeCowd(base.length, [
    [4 * MiB + 3584, Buffer.alloc(512, 0x63)],
    [0x40000, Buffer.alloc(512, 0x74)],
  ]),
);
write(
  "latest-sesparse.vmdk",
  makeSesparse(base.length, [
    [3 * MiB, "unmapped"],
    [3 * MiB + 4096, "zero"],
    [5 * MiB, Buffer.alloc(4096, 0x85)],
  ]),
);

// Create independent logical oracles by modifying the unencoded data.
base.fill(0x63, 4 * MiB + 3584, 4 * MiB + 4096);
base.fill(0x74, 0x40000, 0x40200);
write("middle-expected.img", base);
base.fill(0, 3 * MiB, 3 * MiB + 8192);
base.fill(0x85, 5 * MiB, 5 * MiB + 4096);
write("expected.img", base);
for (const [file, raw] of [
  ["point.vmdk", "middle-expected.img"],
  ["latest.vmdk", "expected.img"],
]) {
  console.log(
    execFileSync(qemu, ["compare", "-f", "vmdk", "-F", "raw", file, raw], {
      cwd: output,
      encoding: "utf8",
      windowsHide: true,
    }).trim(),
  );
}
const manifest = { files, logicalSha256: sha(base) };
write("esxi-chain.json", JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));

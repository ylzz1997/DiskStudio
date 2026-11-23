// Generates an independent QEMU fixture from the project's synthetic NTFS disk.
// All QEMU arguments refer to ordinary files under .vm.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { root, safePath } from "./lab.mjs";

const source = safePath(path.join(root, "payload/ImageBrowse/disk-gpt512.img"));
const output = safePath(path.join(root, "vmdk-chain-fixture"), root, false);
if (
  !fs.lstatSync(source).isFile() ||
  fs.statSync(source).size !== 6 * 1024 ** 2
) {
  throw Error(
    "Run scripts/prepare-image-browse.mjs to generate the synthetic NTFS disk first.",
  );
}
fs.mkdirSync(output);
const raw = fs.readFileSync(source);
raw.fill(0x42, 0x60000, 0x70000);
fs.writeFileSync(path.join(output, "base.raw"), raw, { flag: "wx" });
const run = (tool, ...args) => {
  const command = safePath(path.join(root, `tools/qemu-10.2/qemu-${tool}.exe`));
  console.log(
    execFileSync(command, args, {
      cwd: output,
      encoding: "utf8",
      windowsHide: true,
    }).trim(),
  );
};
run("img", "convert", "-f", "raw", "-O", "vmdk", "base.raw", "base.vmdk");
run(
  "img",
  "create",
  "-f",
  "vmdk",
  "-F",
  "vmdk",
  "-b",
  "base.vmdk",
  "-o",
  "zeroed_grain=on",
  "middle.vmdk",
);
run(
  "io",
  "-f",
  "vmdk",
  "-c",
  "write -P 0x63 0x40000 64k",
  "-c",
  "write -z 0x60000 64k",
  "middle.vmdk",
);
run(
  "img",
  "create",
  "-f",
  "vmdk",
  "-F",
  "vmdk",
  "-b",
  "middle.vmdk",
  "-o",
  "subformat=twoGbMaxExtentSparse",
  "latest.vmdk",
);
run("io", "-f", "vmdk", "-c", "write -P 0x74 0x90000 64k", "latest.vmdk");
raw.fill(0x63, 0x40000, 0x50000);
raw.fill(0, 0x60000, 0x70000);
raw.fill(0x74, 0x90000, 0xa0000);
fs.writeFileSync(path.join(output, "expected.img"), raw, { flag: "wx" });
run("img", "check", "-f", "vmdk", "latest.vmdk");
run("img", "compare", "-f", "vmdk", "-F", "raw", "latest.vmdk", "expected.img");
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const manifest = {
  files: Object.fromEntries(
    ["base.vmdk", "middle.vmdk", "latest.vmdk", "latest-s001.vmdk"].map(
      (name) => [name, sha(fs.readFileSync(path.join(output, name)))],
    ),
  ),
  logicalSha256: sha(raw),
};
fs.writeFileSync(
  path.join(output, "vmdk-chain.json"),
  JSON.stringify(manifest, null, 2) + "\n",
  { flag: "wx" },
);
console.log(JSON.stringify(manifest));

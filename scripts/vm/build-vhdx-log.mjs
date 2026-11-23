// Use ordinary synthetic files only. Verify replay independently on a copy.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  get32,
  get64,
  logEntry,
  markActiveLog,
  MiB,
  K,
} from "../../tests/fixtures/vhdx-data.ts";
import { root, safePath } from "./lab.mjs";

const source = safePath(path.join(root, "payload/ImageBrowse/disk-gpt512.img"));
const output = safePath(path.join(root, "vhdx-log-fixture"), root, false);
const qemu = safePath(path.join(root, "tools/qemu-10.2/qemu-img.exe"));
const raw = fs.readFileSync(source);
if (raw.length !== 6 * MiB)
  throw Error("Expected the synthetic 6 MiB NTFS disk image");
fs.mkdirSync(output);
const run = (...args) =>
  console.log(
    execFileSync(qemu, args, {
      cwd: output,
      encoding: "utf8",
      windowsHide: true,
    }).trim(),
  );
const read = (name) => fs.readFileSync(safePath(path.join(output, name)));
const write = (name, data) =>
  fs.writeFileSync(path.join(output, name), data, { flag: "wx" });
run(
  "convert",
  "-f",
  "raw",
  "-O",
  "vhdx",
  "-o",
  "block_size=1048576",
  source,
  "committed.vhdx",
);
run("compare", "-f", "vhdx", "-F", "raw", "committed.vhdx", source);

const dirty = read("committed.vhdx");
const sector = 4096;
const table = 192 * K;
const batGuid = "6677c22d23f600429d64115e9bfd4a08";
const regions = Array.from(
  { length: get32(dirty, table + 8) },
  (_, i) => table + 16 + i * 32,
);
const batEntry = regions.find(
  (at) => dirty.subarray(at, at + 16).toString("hex") === batGuid,
);
if (batEntry === undefined || dirty.length > 16 * MiB)
  throw Error("Unexpected QEMU fixture layout");
const bat = Number(get64(dirty, batEntry + 16));
if ((get64(dirty, bat) & 7n) !== 6n || (get64(dirty, bat + 8) & 7n) !== 6n)
  throw Error("Expected allocated GPT and NTFS blocks");
const correct = Uint8Array.from(dirty.subarray(bat, bat + sector));
const onlyGpt = correct.slice();
onlyGpt.fill(0, 8, 16);
dirty.fill(0, bat, bat + sector);
markActiveLog(dirty);
const tail = MiB - sector;
logEntry(dirty, tail, tail, 0x100000006n, [{ target: bat, data: onlyGpt }]);
logEntry(dirty, sector, tail, 0x100000007n, [
  { target: bat + sector, data: sector },
  { target: bat, data: correct },
]);
write("pending.vhdx", dirty);
write("replayed-by-qemu.vhdx", dirty);
run("check", "-f", "vhdx", "-r", "all", "replayed-by-qemu.vhdx");
run("compare", "-f", "vhdx", "-F", "raw", "replayed-by-qemu.vhdx", source);
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const manifest = {
  files: { "pending.vhdx": sha(dirty) },
  logicalSha256: sha(raw),
};
write("vhdx-log.json", JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));

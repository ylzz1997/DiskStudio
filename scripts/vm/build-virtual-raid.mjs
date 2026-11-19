// Form conventional left-symmetric RAID5 from the checked 48 MiB Linux LV.
import { createHash } from "node:crypto";
import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { root, safePath } from "./lab.mjs";

const stripe = 64 * 1024;
const offset = 1024 * 1024;
const source = readFileSync(safePath(join(root, "virtual-raid-original.raw")));
if (source.length !== 48 * offset || source.readUInt16LE(1080) !== 0xef53) {
  throw Error("Expected the 48 MiB Ext4 logical fixture");
}
const output = safePath(join(root, "virtual-raid-native"), root, false);
mkdirSync(output);
const members = Array.from({ length: 3 }, () => Buffer.alloc(offset + source.length / 2));
for (let row = 0; row < source.length / (stripe * 2); row++) {
  const parity = 2 - (row % 3);
  const first = source.subarray(row * stripe * 2, (row * 2 + 1) * stripe);
  const second = source.subarray((row * 2 + 1) * stripe, (row * 2 + 2) * stripe);
  const start = offset + row * stripe;
  first.copy(members[(parity + 1) % 3], start);
  second.copy(members[(parity + 2) % 3], start);
  for (let i = 0; i < stripe; i++) {
    members[parity][start + i] = first[i] ^ second[i];
  }
}
const hash = (data) => createHash("sha256").update(data).digest("hex");
const files = members.map((member, index) => {
  const name = `Virtual-Raid-${index + 1}.raw`;
  writeFileSync(join(output, name), member, { flag: "wx" });
  return { name, sha256: hash(member) };
});
const manifest = { logicalSha256: hash(source), files };
writeFileSync(join(output, "manifest.json"), JSON.stringify(manifest, null, 2), { flag: "wx" });
console.log(manifest);

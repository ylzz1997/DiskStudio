// Use QEMU as an independent decoder for each expandable HDS. The logical
// oracle is built from the original RAW and ordered writes, before encoding.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  makeHds,
  withDirtyBitmap,
} from "../../tests/fixtures/parallels-bundle.ts";
import { root, safePath } from "./lab.mjs";

const MiB = 1024 ** 2;
const cluster = 64 * 1024;
const split = 2 * MiB;
const history = process.argv.slice(2).includes("--history");
const batch = history ? "parallels-history" : "parallels-bundle";
const output = safePath(path.join(root, batch + "-fixture"), root, false);
const qemu = safePath(path.join(root, "tools/qemu-10.2/qemu-img.exe"));
const source = safePath(path.join(root, "payload/ImageBrowse/disk-gpt512.img"));
const base = fs.readFileSync(source);
if (base.length !== 6 * MiB) throw Error("Expected the 6 MiB GPT/NTFS fixture");

fs.mkdirSync(output);
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const files = {};
const isData = new Set(["DiskDescriptor.xml", "lower-base.hdd"]);
function put(name, bytes, record = isData.has(name) || name.endsWith(".hds")) {
  fs.writeFileSync(path.join(output, name), bytes, { flag: "wx" });
  if (record) files[name] = sha(bytes);
}

const zero = "{00000000-0000-0000-0000-000000000000}";
const ids = [
  "{91e5dfa1-751f-47e4-8bc0-c0f608edcc62}",
  "{72f99090-4c85-4d25-be90-1e2e96d7cdb2}",
  "{193ff19c-4618-4af3-b592-15c3d7164f83}",
  "{5fbaabe3-6958-40ff-92a7-860e329aab41}",
];
const parents = [zero, ids[0], ids[1], ids[0]];
function storage(label, start, end, raw) {
  const resources = [
    raw ? label + "-base.hdd" : label + "-base.hds",
    label + "-point.hds",
    label + "-latest.hds",
    label + (history ? "-historical.hds" : "-unselected.hds"),
  ];
  const images = [2, 0, 3, 1].map(
    (i) =>
      "<Image><GUID>" +
      ids[i] +
      "</GUID><Type>" +
      (i === 0 && raw ? "Plain" : "Compressed") +
      "</Type><Flags/><File>" +
      (i === 0 ? "copied-from/Offline.hdd/" : "") +
      resources[i] +
      "</File></Image>",
  );
  return (
    "<Storage><Start>" +
    start / 512 +
    "</Start><End>" +
    end / 512 +
    "</End><Blocksize>" +
    cluster / 512 +
    "</Blocksize>\n" +
    images.join("\n") +
    "\n</Storage>"
  );
}
const xml =
  [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<Parallels_disk_image Version="1.0">',
    "<Disk_Parameters><Disk_size>12288</Disk_size><Cylinders>12</Cylinders><Heads>16</Heads><Sectors>64</Sectors><Padding>0</Padding></Disk_Parameters>",
    "<StorageData>",
    storage("upper", split, base.length, false),
    storage("lower", 0, split, true),
    "</StorageData>",
    "<Snapshots><TopGUID>" + ids[2] + "</TopGUID>",
    ...[3, 1, 0, 2].map(
      (i) =>
        "<Shot><GUID>" +
        ids[i] +
        "</GUID><ParentGUID>" +
        parents[i] +
        "</ParentGUID></Shot>",
    ),
    "</Snapshots></Parallels_disk_image>",
  ].join("\n") + "\n";
put("DiskDescriptor.xml", xml);

base.fill(0x40, 3 * MiB, 3 * MiB + cluster);
base.fill(0x51, 0x40000, 0x40000 + cluster);
put("lower-base.hdd", base.subarray(0, split));
const upper = base.subarray(split);
const upperBaseBlocks = [];
for (let off = 0; off < upper.length; off += cluster) {
  const block = upper.subarray(off, off + cluster);
  if (block.some(Boolean)) upperBaseBlocks.push([off, block]);
}

const hdsChecks = [];
function hds(name, length, blocks, legacy, bitmap = false) {
  const image = makeHds(length, cluster, blocks, legacy);
  put(name, bitmap ? withDirtyBitmap(image, cluster, length / 512) : image);
  const logical = Buffer.alloc(length);
  for (const [offset, bytes] of blocks) logical.set(bytes, offset);
  const oracle = name + ".img";
  put(oracle, logical, false);
  const result = execFileSync(
    qemu,
    ["compare", "-f", "parallels", "-F", "raw", name, oracle],
    {
      cwd: output,
      encoding: "utf8",
      windowsHide: true,
    },
  ).trim();
  hdsChecks.push(name);
  console.log(name + " " + result);
}
hds("upper-base.hds", upper.length, upperBaseBlocks, false);
hds("lower-point.hds", split, [[0x60000, Buffer.alloc(cluster, 0x62)]], true);
hds(
  "upper-point.hds",
  upper.length,
  [[2 * MiB, Buffer.alloc(cluster, 0x73)]],
  false,
);
hds("lower-latest.hds", split, [[0x40000, Buffer.alloc(cluster)]], false);
hds(
  "upper-latest.hds",
  upper.length,
  [
    [MiB, Buffer.alloc(cluster)],
    [3 * MiB, Buffer.alloc(cluster, 0x84)],
  ],
  false,
);

const current = Buffer.from(base);
current.fill(0x62, 0x60000, 0x60000 + cluster);
current.fill(0x73, 4 * MiB, 4 * MiB + cluster);
current.fill(0, 0x40000, 0x40000 + cluster);
current.fill(0, 3 * MiB, 3 * MiB + cluster);
current.fill(0x84, 5 * MiB, 5 * MiB + cluster);
let expected = current;
if (history) {
  hds(
    "lower-historical.hds",
    split,
    [[0x80000, Buffer.alloc(cluster, 0x96)]],
    false,
    true,
  );
  hds(
    "upper-historical.hds",
    upper.length,
    [[2 * MiB + 0x80000, Buffer.alloc(cluster, 0xa8)]],
    false,
  );
  expected = Buffer.from(base);
  expected.fill(0x96, 0x80000, 0x80000 + cluster);
  expected.fill(0xa8, 4 * MiB + 0x80000, 4 * MiB + 0x80000 + cluster);
  put("current-expected.img", current);
}
put("expected.img", expected);
const manifest = {
  files,
  logicalSha256: sha(expected),
  hdsChecks,
  ...(history
    ? {
        snapshot: ids[3].slice(1, -1),
        currentSnapshot: ids[2].slice(1, -1),
        currentSha256: sha(current),
      }
    : {}),
};
put(batch + ".json", JSON.stringify(manifest, null, 2) + "\n");
console.log(JSON.stringify(manifest));

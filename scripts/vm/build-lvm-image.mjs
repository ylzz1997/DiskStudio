import { closeSync, fstatSync, ftruncateSync, openSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";
import { crc32 } from "node:zlib";
import { spawnSync } from "node:child_process";
import { root, safePath } from "./lab.mjs";

const modes = new Map([
  [undefined, { directory: "lvm-native", name: "Lvm-Group", count: 2 }],
  ["--raid", { directory: "lvm-raid-native", name: "Lvm-Raid-Group", count: 5 }],
  ["--thin", { directory: "lvm-thin-native", name: "Lvm-Thin-Group", count: 2 }],
  ["--cow", { directory: "lvm-cow-native", name: "Lvm-Cow-Group", count: 2 }],
  ["--raid10", { directory: "lvm-raid10-native", name: "Lvm-Raid10-Group", count: 4 }],
]);
const mode = modes.get(process.argv[2]);
if (!mode || process.argv.length > 3) throw Error("Expected --raid, --thin, --cow, --raid10 or no argument");
const { directory, name, count } = mode;
const dir = safePath(join(root, directory));
const fixture = (name, exists = true) => safePath(join(dir, name), dir, exists);
const qemu = safePath(join(root, "tools/qemu/qemu-img.exe"));
const raw = fixture(`${name}.raw`, false);
const container = fixture(`${name}.vhdx`, false);
const sector = 512;
const mib = 1024 ** 2;
const memberBytes = 256 * mib;
const size = count * memberBytes + (count + 2) * mib;
const last = size / sector - 1;
const starts = Array.from({ length: count }, (_, index) => mib + index * (memberBytes + mib));
const entries = Buffer.alloc(128 * 128);
const out = openSync(raw, "wx");
function write(bytes, offset) {
  if (writeSync(out, bytes, 0, bytes.length, offset) !== bytes.length) {
    throw new Error("Short LVM2 fixture write");
  }
}
try {
  ftruncateSync(out, size);
  const mbr = Buffer.alloc(sector);
  mbr[450] = 0xee;
  mbr.writeUInt32LE(1, 454);
  mbr.writeUInt32LE(last, 458);
  mbr.writeUInt16LE(0xaa55, 510);
  write(mbr, 0);
  starts.forEach((start, index) => {
    const entry = entries.subarray(index * 128, (index + 1) * 128);
    // Linux LVM type GUID in GPT byte order.
    Buffer.from("79d3d6e607f5c244a23c238f2a3df928", "hex").copy(entry);
    entry[16] = index + 1;
    entry.writeBigUInt64LE(BigInt(start / sector), 32);
    entry.writeBigUInt64LE(BigInt((start + memberBytes) / sector - 1), 40);
    entry.write(`PV ${index + 1}`, 56, "utf16le");
    const pv = openSync(fixture(`pv-${index + 1}.raw`), "r");
    try {
      if (fstatSync(pv).size !== memberBytes) throw new Error("Unexpected PV length");
      const bytes = Buffer.alloc(mib);
      for (let offset = 0; offset < memberBytes; offset += bytes.length) {
        if (readSync(pv, bytes, 0, bytes.length, offset) !== bytes.length) {
          throw new Error("Short LVM2 fixture read");
        }
        if (bytes.some((byte) => byte !== 0)) write(bytes, start + offset);
      }
    } finally {
      closeSync(pv);
    }
  });
  const tableSectors = entries.length / sector;
  for (const [current, alternate, table] of [[1, last, 2], [last, 1, last - tableSectors]]) {
    write(entries, table * sector);
    const header = Buffer.alloc(sector);
    header.write("EFI PART", 0);
    header.writeUInt32LE(0x10000, 8);
    header.writeUInt32LE(92, 12);
    header.writeBigUInt64LE(BigInt(current), 24);
    header.writeBigUInt64LE(BigInt(alternate), 32);
    header.writeBigUInt64LE(BigInt(2 + tableSectors), 40);
    header.writeBigUInt64LE(BigInt(last - tableSectors - 1), 48);
    header[56] = 3;
    header.writeBigUInt64LE(BigInt(table), 72);
    header.writeUInt32LE(128, 80);
    header.writeUInt32LE(128, 84);
    header.writeUInt32LE(crc32(entries), 88);
    header.writeUInt32LE(crc32(header.subarray(0, 92)), 16);
    write(header, current * sector);
  }
} finally {
  closeSync(out);
}
for (const args of [
  ["convert", "-f", "raw", "-O", "vhdx", "-o", "block_size=1048576", raw, container],
  ["compare", "-f", "raw", "-F", "vhdx", raw, container],
]) {
  const result = spawnSync(qemu, args, { stdio: "inherit", windowsHide: true, timeout: 60000 });
  if (result.status !== 0) throw new Error("QEMU could not verify the ordinary LVM2 image");
}

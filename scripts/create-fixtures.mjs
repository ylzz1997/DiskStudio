// Creates small, synthetic regular files only. No device paths or system tools.
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const destination = new URL('../fixtures/generated/', import.meta.url);
await mkdir(destination, { recursive: true });
const MiB = 1024 ** 2;
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) { crc ^= byte; for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0); }
  return (crc ^ 0xffffffff) >>> 0;
}
const mbr = Buffer.alloc(8 * MiB);
mbr[510] = 0x55; mbr[511] = 0xaa; mbr[450] = 0x83;
mbr.writeUInt32LE(2048, 454); mbr.writeUInt32LE(8192, 458);
const gpt = Buffer.alloc(8 * MiB); const lastLba = gpt.length / 512 - 1;
gpt[510] = 0x55; gpt[511] = 0xaa; gpt[450] = 0xee;
gpt.writeUInt32LE(1, 454); gpt.writeUInt32LE(lastLba, 458);
const table = Buffer.alloc(128 * 128);
Buffer.from('a2a0d0ebe5b9334487c068b6b72699c7', 'hex').copy(table, 0);
Buffer.from('00112233445566778899aabbccddeeff', 'hex').copy(table, 16);
table.writeBigUInt64LE(2048n, 32); table.writeBigUInt64LE(10239n, 40);
table.write('示例分区', 56, 'utf16le');
function header(current, alternate, tableLba) {
  const h = Buffer.alloc(512); h.write('EFI PART'); h.writeUInt32LE(0x10000, 8); h.writeUInt32LE(92, 12);
  h.writeBigUInt64LE(BigInt(current), 24); h.writeBigUInt64LE(BigInt(alternate), 32);
  h.writeBigUInt64LE(34n, 40); h.writeBigUInt64LE(BigInt(lastLba - 33), 48);
  Buffer.from('11223344556677889900aabbccddeeff', 'hex').copy(h, 56);
  h.writeBigUInt64LE(BigInt(tableLba), 72); h.writeUInt32LE(128, 80); h.writeUInt32LE(128, 84);
  h.writeUInt32LE(crc32(table), 88); h.writeUInt32LE(crc32(h.subarray(0, 92)), 16); return h;
}
header(1, lastLba, 2).copy(gpt, 512); table.copy(gpt, 1024);
table.copy(gpt, (lastLba - 32) * 512); header(lastLba, 1, lastLba - 32).copy(gpt, lastLba * 512);
const corrupt = Buffer.from(gpt); corrupt[528] ^= 1;
const badBackup = Buffer.from(gpt); badBackup[lastLba * 512 + 16] ^= 1;
const conflict = Buffer.from(gpt);
const backupAt = (lastLba - 32) * 512;
conflict.write('另一份表', backupAt + 56, 'utf16le');
conflict.writeUInt32LE(crc32(conflict.subarray(backupAt, backupAt + table.length)), lastLba * 512 + 88);
conflict.writeUInt32LE(0, lastLba * 512 + 16);
conflict.writeUInt32LE(crc32(conflict.subarray(lastLba * 512, lastLba * 512 + 92)), lastLba * 512 + 16);
const logical = Buffer.alloc(16 * MiB);
function mbrEntry(bytes, tableAt, slot, type, start, length) {
  bytes[tableAt + 510] = 0x55; bytes[tableAt + 511] = 0xaa;
  const entry = tableAt + 446 + slot * 16;
  bytes[entry + 4] = type; bytes.writeUInt32LE(start, entry + 8); bytes.writeUInt32LE(length, entry + 12);
}
mbrEntry(logical, 0, 0, 7, 2048, 2048);
mbrEntry(logical, 0, 1, 0x0f, 8192, 16384);
mbrEntry(logical, 4 * MiB, 0, 0x83, 2048, 2048);
mbrEntry(logical, 4 * MiB, 1, 0x0f, 8192, 8192);
mbrEntry(logical, 8 * MiB, 0, 0x0c, 2048, 2048);
const loop = Buffer.from(logical);
mbrEntry(loop, 8 * MiB, 1, 0x0f, 0, 2048);
for (const [filename, bytes] of [['mbr-example.img', mbr], ['gpt-example.img', gpt], ['gpt-damaged.img', corrupt], ['gpt-backup-damaged.img', badBackup], ['gpt-conflicting-tables.img', conflict], ['mbr-logical.img', logical], ['mbr-ebr-loop.img', loop]]) {
  try { await writeFile(new URL(filename, destination), bytes, { flag: 'wx' }); console.log(`Created ${filename} (${bytes.length / MiB} MiB)`); }
  catch (e) { if (e.code !== 'EEXIST') throw e; console.log(`Kept existing ${filename}`); }
}
console.log(`Fixtures: ${fileURLToPath(destination)}`);
console.log('Partition tables only; no filesystem or personal data. Open these files in DiskStudio; do not mount them.');

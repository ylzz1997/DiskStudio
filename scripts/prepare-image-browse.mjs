// Synthetic ordinary files only. No devices, mounts or guest execution.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const stage = path.join(project, '.vm', 'payload', 'ImageBrowse');
for (const p of [project, path.join(project, '.vm'), path.dirname(stage)]) {
  if (fs.lstatSync(p).isSymbolicLink() || fs.realpathSync(p).toLowerCase() !== p.toLowerCase()) throw Error('Fixture parent must be a real project directory');
}
fs.mkdirSync(stage); // Deliberately refuse to overwrite an earlier corpus.
const source = fs.readFileSync(path.join(project, 'tests/fixtures/ntfs-testfs1.img'));
const MiB = 1024 ** 2;
function crc(bytes) {
  let c = 0xffffffff;
  for (const b of bytes) {
    c ^= b;
    for (let i = 0; i < 8; i++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return (~c) >>> 0;
}
function mbr(b, type, start, length) {
  b[510] = 0x55; b[511] = 0xaa; b[450] = type;
  b.writeUInt32LE(start, 454); b.writeUInt32LE(length, 458);
}
function gpt(sector) {
  const b = Buffer.alloc(source.length + 4 * MiB);
  const last = b.length / sector - 1;
  mbr(b, 0xee, 1, last);
  const entries = Buffer.alloc(16384);
  entries.fill(1, 0, 16); entries.fill(2, 16, 32);
  entries.writeBigUInt64LE(BigInt(MiB / sector), 32);
  entries.writeBigUInt64LE(BigInt((MiB + source.length) / sector - 1), 40);
  Buffer.from('NTFS fixture', 'utf16le').copy(entries, 56);
  for (const [own, other, array] of [[1, last, 2], [last, 1, last - 16384 / sector]]) {
    entries.copy(b, array * sector);
    const h = b.subarray(own * sector, (own + 1) * sector);
    h.write('EFI PART'); h.writeUInt32LE(0x10000, 8); h.writeUInt32LE(92, 12);
    for (const [at, n] of [[24,own],[32,other],[40,2+16384/sector],[48,last-16384/sector-1],[72,array]]) h.writeBigUInt64LE(BigInt(n), at);
    h.fill(3,56,72); h.writeUInt32LE(128,80); h.writeUInt32LE(128,84); h.writeUInt32LE(crc(entries),88); h.writeUInt32LE(crc(h.subarray(0,92)),16);
  }
  source.copy(b, MiB);
  return b;
}
const mbrDisk = Buffer.alloc(source.length + 4 * MiB);
mbr(mbrDisk, 7, MiB / 512, source.length / 512); source.copy(mbrDisk, MiB);
const bad = gpt(512); bad[528] ^= 1;
const files = [
  ['partition-ntfs.img', source, 'partition', 512],
  ['disk-mbr.img', mbrDisk, 'disk', 512],
  ['disk-gpt512.img', gpt(512), 'disk', 512],
  ['disk-gpt4096.img', gpt(4096), 'disk', 4096],
  ['rejected-crc.img', bad, 'disk', 512],
];
const manifest = files.map(([name, data, kind, sector]) => {
  fs.writeFileSync(path.join(stage, name), data, { flag: 'wx' });
  return { name, size: data.length, sha256: crypto.createHash('sha256').update(data).digest('hex'), kind, sector };
});
fs.writeFileSync(path.join(stage, 'manifest.json'), JSON.stringify({ files: manifest, expectedFile: { name: 'file-with-12345', utf8: '12345', sha256: crypto.createHash('sha256').update('12345').digest('hex') } }, null, 2), { flag: 'wx' });
console.log(JSON.stringify({ folder: stage, files: manifest }, null, 2));

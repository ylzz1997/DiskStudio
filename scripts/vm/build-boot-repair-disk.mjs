// Seed a new, unattached disposable QCOW2 from the regular 4Kn test image.
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { crc32 } from 'node:zlib';
import { checkQcowHeader, root, safePath } from './lab.mjs';

const sector = 4096;
const capacity = 16 * 1024 ** 3;
const seed = safePath(path.join(root, 'payload/Boot-Repair-4096.img'));
const destination = safePath(path.join(root, 'windows/test-b-boot-repair-4kn.qcow2'), root, false);
if (fs.existsSync(destination)) throw new Error('The dedicated Test B already exists');
const temp = fs.mkdtempSync(safePath(path.join(root, 'boot-repair-seed-'), root, false));
const staged = path.join(temp, 'test-b.qcow2');
const bytes = fs.readFileSync(seed);
const sourceLast = bytes.length / sector - 1;
const last = capacity / sector - 1;
const tableSectors = 4;
const backupStart = last - tableSectors;
const table = bytes.subarray(2 * sector, (2 + tableSectors) * sector);

function checkHeader(header, start, peer, entries) {
  const copy = Buffer.from(header);
  copy.fill(0, 16, 20);
  if (header.subarray(0, 8).toString() !== 'EFI PART'
      || header.readUInt32LE(12) !== 92
      || header.readUInt32LE(16) !== crc32(copy.subarray(0, 92))
      || header.readBigUInt64LE(24) !== BigInt(start)
      || header.readBigUInt64LE(32) !== BigInt(peer)
      || header.readBigUInt64LE(72) !== BigInt(entries)
      || header.readUInt32LE(80) !== 128
      || header.readUInt32LE(84) !== 128
      || header.readUInt32LE(88) !== crc32(table)) {
    throw new Error('Boot repair seed has an unexpected GPT');
  }
}

if (bytes.length !== 299 * 1024 ** 2
    || bytes.readUInt32LE(458) !== sourceLast
    || !bytes.subarray((sourceLast - tableSectors) * sector, sourceLast * sector).equals(table)) {
  throw new Error('Expected the bounded, mirrored 4Kn boot repair image');
}
const original = bytes.subarray(sector, 2 * sector);
checkHeader(original, 1, sourceLast, 2);
checkHeader(bytes.subarray(sourceLast * sector), sourceLast, 1, sourceLast - tableSectors);
for (const [index, type, start, end] of [
  [0, '28732ac11ff8d211ba4b00a0c93ec93b', 256, 69887],
  [1, 'a2a0d0ebe5b9334487c068b6b72699c7', 70144, 76287],
]) {
  const entry = table.subarray(index * 128, (index + 1) * 128);
  if (entry.subarray(0, 16).toString('hex') !== type
      || entry.readBigUInt64LE(32) !== BigInt(start)
      || entry.readBigUInt64LE(40) !== BigInt(end)) {
    throw new Error('Expected EFI system and Windows data extents in the seed');
  }
}
if (table.subarray(256).some(byte => byte !== 0)) throw new Error('Unexpected extra seed GPT entries');

function relocatedHeader(start, peer, entries) {
  const header = Buffer.from(original);
  header.writeBigUInt64LE(BigInt(start), 24);
  header.writeBigUInt64LE(BigInt(peer), 32);
  header.writeBigUInt64LE(BigInt(backupStart - 1), 48);
  header.writeBigUInt64LE(BigInt(entries), 72);
  header.writeUInt32LE(0, 16);
  header.writeUInt32LE(crc32(header.subarray(0, 92)), 16);
  return header;
}

const mbr = Buffer.from(bytes.subarray(0, sector));
mbr.writeUInt32LE(last, 458);
const primary = Buffer.concat([mbr, relocatedHeader(1, last, 2)]);
const backup = Buffer.concat([table, relocatedHeader(last, 1, backupStart)]);
fs.writeFileSync(path.join(temp, 'primary.raw'), primary, { flag: 'wx' });
fs.writeFileSync(path.join(temp, 'backup.raw'), backup, { flag: 'wx' });

function qemu(tool, args) {
  const exe = safePath(path.join(root, 'tools/qemu', `${tool}.exe`));
  const result = spawnSync(exe, args, { cwd: temp, windowsHide: true, encoding: 'utf8', timeout: 120000 });
  if (result.error || result.status !== 0) throw new Error(result.error?.message ?? result.stderr ?? result.stdout);
  return result.stdout;
}

qemu('qemu-img', ['convert', '-f', 'raw', '-O', 'qcow2', '-c', seed, staged]);
qemu('qemu-img', ['resize', '-f', 'qcow2', staged, String(capacity)]);
qemu('qemu-io', [
  '-f', 'qcow2',
  '-c', `write -s primary.raw 0 ${primary.length}`,
  '-c', `write -z ${(sourceLast - tableSectors) * sector} ${backup.length}`,
  '-c', `write -s backup.raw ${backupStart * sector} ${backup.length}`,
  staged,
]);
for (const [name, start, expected] of [['primary', 0, primary], ['backup', backupStart, backup]]) {
  // qemu-img applies count to the input extent before skipping its prefix.
  const end = start + expected.length / sector;
  qemu('qemu-img', ['dd', '-f', 'qcow2', `if=${staged}`, `of=${name}.readback`, `bs=${sector}`, `skip=${start}`, `count=${end}`]);
  if (!fs.readFileSync(path.join(temp, `${name}.readback`)).equals(expected)) {
    throw new Error(`Seed ${name} GPT readback differs`);
  }
}
if (checkQcowHeader(staged) !== capacity) throw new Error('Expected a standalone 16 GiB QCOW2');
qemu('qemu-img', ['check', '-f', 'qcow2', staged]);
fs.copyFileSync(staged, destination, fs.constants.COPYFILE_EXCL);
console.log(JSON.stringify({
  file: destination,
  bytes: capacity,
  seedSha256: createHash('sha256').update(bytes).digest('hex'),
  imageSha256: createHash('sha256').update(fs.readFileSync(destination)).digest('hex'),
  secondGptLba: last,
}));

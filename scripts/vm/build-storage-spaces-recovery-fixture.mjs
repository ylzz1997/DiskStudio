import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { crc32, gunzipSync } from 'node:zlib';
import { root, safePath } from './lab.mjs';

const [captureName, cacheName, option] = process.argv.slice(2);
const thin = option === '--thin';
const cacheNamePattern = thin ? /^storage-spaces-internal-\d+$/ : /^storage-spaces-cache-complete-\d+$/;
if ((!thin && process.argv.length !== 4) || (thin && process.argv.length !== 5) ||
    !/^storage-spaces-witness(?:-full)?-\d+$/.test(captureName ?? '') || !cacheNamePattern.test(cacheName ?? '')) {
  throw Error('Expected the verified Storage Spaces witness and complete cache capture directories');
}
const capture = safePath(path.join(root, captureName));
const cacheCapture = safePath(path.join(root, cacheName));
const output = safePath(path.join(root, thin ? 'storage-spaces-recovery-thin.json' : 'storage-spaces-recovery-clean-cache.json'), root, false);
const manifest = JSON.parse(fs.readFileSync(safePath(path.join(capture, 'manifest.json')), 'utf8'));
const cacheManifest = JSON.parse(fs.readFileSync(safePath(path.join(cacheCapture, 'manifest.json')), 'utf8'));
if (!Array.isArray(manifest) || manifest.length !== 2 || manifest[0].slot !== 'A' || manifest[1].slot !== 'B') {
  throw Error('Expected a verified pair of Storage Spaces members');
}
const sector = 4096;
const mib = 1024 ** 2;
const logicalSize = 1024 * mib;
const partitionStart = thin ? 512 * mib - 23 * sector : mib;
const partitionSize = 64 * mib;
const lastLba = logicalSize / sector - 1;
const backupTable = lastLba - (thin ? 31 : 4);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');

// A FAT16 partition with one existing file and one unallocated deleted file.
const fat = Buffer.alloc((thin ? 48 : 32) * sector);
fat.set([0xeb, 0x3c, 0x90]);
fat.write('MSDOS5.0', 3, 'ascii');
fat.writeUInt16LE(sector, 11);
fat[13] = 1;
fat.writeUInt16LE(1, 14);
fat[16] = 2;
fat.writeUInt16LE(512, 17);
fat.writeUInt16LE(partitionSize / sector, 19);
fat[21] = 0xf8;
fat.writeUInt16LE(8, 22);
fat.writeUInt16LE(32, 24);
fat.writeUInt16LE(64, 26);
fat.writeUInt32LE(partitionStart / sector, 28);
fat[36] = 0x80;
fat[38] = 0x29;
fat.writeUInt32LE(0x54020951, 39);
fat.write('DS SPACES  ', 43, 'ascii');
fat.write('FAT16   ', 54, 'ascii');
fat.writeUInt16LE(0xaa55, 510);
for (const start of [sector, 9 * sector]) {
  for (const [cluster, next] of [0xfff8, 0xffff, 3, 4, 0xffff].entries()) {
    fat.writeUInt16LE(next, start + cluster * 2);
  }
}
function file(name, cluster, length, index, deleted) {
  const offset = 17 * sector + 32 * index;
  const bytes = Buffer.alloc(length);
  for (let i = 0; i < length; i++) bytes[i] = (i * 23 + (i >>> 4) + index * 53) & 255;
  fat.write(name, offset, 'ascii');
  if (deleted) fat[offset] = 0xe5;
  fat[offset + 11] = 0x20;
  fat.writeUInt16LE(0x5d76, offset + 24);
  fat.writeUInt16LE(cluster, offset + 26);
  fat.writeUInt32LE(length, offset + 28);
  bytes.copy(fat, (21 + cluster - 2) * sector);
  return { bytes: length, sha256: sha256(bytes) };
}
const existing = file('WITNESS BIN', 2, 8233, 0, false);
const deleted = thin ? file('ERASED  BIN', 20, 3601, 1, true) : file('ERASED  BIN', 6, 5709, 1, true);

const table = Buffer.alloc(4 * sector);
Buffer.from('a2a0d0ebe5b9334487c068b6b72699c7', 'hex').copy(table);
Buffer.from('3b02f1c4d3109f4cb8d470cd0bc94ff1', 'hex').copy(table, 16);
table.writeBigUInt64LE(BigInt(partitionStart / sector), 32);
table.writeBigUInt64LE(BigInt((partitionStart + partitionSize) / sector - 1), 40);
table.write('Storage Spaces test files', 56, 'utf16le');
function header(own, other, entries) {
  const data = Buffer.alloc(sector);
  data.write('EFI PART');
  data.writeUInt32LE(0x10000, 8);
  data.writeUInt32LE(92, 12);
  for (const [offset, value] of [[24, own], [32, other], [40, 6], [48, backupTable - 1], [72, entries]]) {
    data.writeBigUInt64LE(BigInt(value), offset);
  }
  Buffer.from('7396a4ccc207014098248224d399af41', 'hex').copy(data, 56);
  data.writeUInt32LE(128, 80);
  data.writeUInt32LE(128, 84);
  data.writeUInt32LE(crc32(table), 88);
  data.writeUInt32LE(crc32(data.subarray(0, 92)), 16);
  return data;
}
const mbr = Buffer.alloc(sector);
mbr[450] = 0xee;
mbr.writeUInt32LE(1, 454);
mbr.writeUInt32LE(lastLba, 458);
mbr.writeUInt16LE(0xaa55, 510);
const extents = [
  [0, Buffer.concat([mbr, header(1, lastLba, 2), table])],
  [partitionStart, fat],
  [backupTable * sector, table],
  [lastLba * sector, header(lastLba, 1, backupTable)],
];

function thinSegments(index, capture, witnesses) {
  // The source uses B/A order for the direct stripes and A/B for the cache.
  const interleave = 64 * 1024;
  const directBase = 0xe1000000;
  const cacheBase = 0xc1000000;
  const markerOffset = index === 0 ? interleave : 0;
  if (!witnesses.some(item => item.kind === 2 && item.logical === markerOffset && item.offset === directBase)) {
    throw Error('Unexpected thin data column mapping');
  }
  if (capture.length !== 4096 * 1024) throw Error('Unexpected capture of thin cache metadata');
  const cache = Buffer.from(capture.subarray(0, (0x480000 / 2)));
  const segments = [];
  for (const [base, bytes] of extents) {
    for (let offset = 0; offset < bytes.length; offset += sector) {
      const logical = base + offset;
      const page = bytes.subarray(offset, offset + sector);
      if (!page.some(byte => byte !== 0)) continue;
      const column = Math.floor(logical / interleave) % 2;
      if (logical < 512 * mib) {
        if (column !== index) segments.push([directBase + Math.floor(logical / (2 * interleave)) * interleave + logical % interleave, page]);
        continue;
      }
      // The captured checkpoint maps these pages to cache slots 0 and 3.
      const window = Math.floor(logical / (2 * interleave)) * 2 * interleave;
      const slot = window === 512 * mib ? 0 : window === logicalSize - 2 * interleave ? 3 : -1;
      const mask = slot === 0 ? 0x00010001 : slot === 3 ? 0x8001001f : 0;
      const pageNumber = (logical - window) / sector;
      if (((mask >>> pageNumber) & 1) === 0) throw Error('The synthetic sector lacks a validated cache slot');
      if (column === index) segments.push([cacheBase + cache.length + slot * interleave + logical % interleave, page]);
    }
  }
  for (let offset = 0; offset < cache.length; offset += sector) {
    const page = cache.subarray(offset, offset + sector);
    if (page.some(byte => byte !== 0)) segments.push([cacheBase + offset, page]);
  }
  return segments;
}

const members = manifest.map((member, index) => {
  const { slot, capacity, regions, witnesses } = member;
  if (capacity !== 16 * 1024 ** 3 || member.sector !== sector || regions.length !== 3 ||
      !Array.isArray(witnesses)) throw Error(`Unexpected member ${slot} geometry`);
  const primary = witnesses.filter(item => item.kind === 1 && item.logical === 0);
  if (primary.length !== 1 || primary[0].offset !== 0x81000000) throw Error('Unexpected fixed mirror allocation');
  const compressed = fs.readFileSync(safePath(path.join(capture, `${slot}.gz`)));
  if (compressed.length !== member.gzipBytes || sha256(compressed) !== member.gzipSha256) throw Error(`Capture ${slot} does not match its manifest`);
  const decoded = gunzipSync(compressed, { maxOutputLength: 2 * mib });
  let position = 0;
  const segments = regions.map((region, index) => {
    const data = decoded.subarray(position, position + region.bytes);
    position += region.bytes;
    if (data.length !== region.bytes || sha256(data) !== region.sha256) throw Error(`Capture ${slot} region mismatch`);
    return [region.offset, index === 2 ? data.subarray(0, 16 * 1024) : data];
  });
  const cached = cacheManifest[index];
  if (cached?.slot !== slot || cached.capacity !== capacity || cached.sector !== sector ||
      cached.regions.length !== regions.length || !cached.regions.every((region, n) =>
        region.sha256 === regions[n].sha256 && region.offset === regions[n].offset && region.bytes === regions[n].bytes) ||
      cached.internal.length !== (thin ? 2 : 5)) throw Error(`Cache ${slot} has a different member snapshot`);
  const cacheBytes = fs.readFileSync(safePath(path.join(cacheCapture, `${slot}.gz`)));
  if (sha256(cacheBytes) !== cached.gzipSha256 || cacheBytes.length !== cached.gzipBytes) throw Error('Cache archive mismatch');
  const cacheDecoded = gunzipSync(cacheBytes, { maxOutputLength: 10 * mib });
  const cacheRegions = cached.internal.map(region => {
    const contents = cacheDecoded.subarray(position, position + region.bytes);
    position += region.bytes;
    if (contents.length !== region.bytes || sha256(contents) !== region.sha256) throw Error('Cache region mismatch');
    return contents;
  });
  if (position !== cacheDecoded.length) throw Error('Extra bytes in cache capture');
  if (thin) {
    if (cached.internal[0].offset !== 0xc1000000) throw Error('Unexpected thin cache allocation');
    segments.push(...thinSegments(index, cacheRegions[0], witnesses));
  } else {
    const nonzero = [];
    for (const [index, contents] of cacheRegions.entries()) {
      for (let offset = 0; offset < contents.length; offset += sector) {
        const page = contents.subarray(offset, offset + sector);
        if (page.some(byte => byte !== 0)) nonzero.push([cached.internal[index].offset + offset, page]);
      }
    }
    if (nonzero.map(([offset]) => offset).join() !== [0x31000000, 0x41000000, 0x41002000, 0x41003000].join()) {
      throw Error('The complete cache capture differs from the clean mirror fixture');
    }
    segments.push(...nonzero);
    for (const [offset, data] of extents.slice(0, 2)) segments.push([primary[0].offset + offset, data]);
    segments.push([primary[0].offset + backupTable * sector, Buffer.concat(extents.slice(2).map(([, data]) => data))]);
  }
  return {
    slot, capacity,
    segments: segments.sort((a, b) => a[0] - b[0]).map(([offset, data]) => ({ offset, sha256: sha256(data), data: data.toString('base64') })),
  };
});
fs.writeFileSync(output, JSON.stringify({ members, existing, deleted }) + '\n', { flag: 'wx' });
console.log(`Prepared bounded Storage Spaces recovery fixture at ${output}`);

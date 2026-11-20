import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { root, safePath } from './lab.mjs';

const [name] = process.argv.slice(2);
const metadata = /^virtual-raid-md-metadata-4kn-[0-9]+\.log$/.test(name ?? '');
const zoned = /^virtual-raid-md-zones-4kn-[0-9]+\.log$/.test(name ?? '');
const raid10 = /^virtual-raid-md10-4kn-[0-9]+\.log$/.test(name ?? '');
const advanced = /^virtual-raid-md10-advanced-4kn-[0-9]+\.log$/.test(name ?? '');
if ((!metadata && !zoned && !raid10 && !advanced && !/^virtual-raid-md6-4kn-[0-9]+\.log$/.test(name ?? '')) || process.argv.length !== 3) {
  throw new Error('Expected the name of a Linux md Ubuntu fixture log');
}
const input = safePath(path.join(root, 'ubuntu', name));
const directory = zoned ? 'virtual-raid-md-zones' : metadata ? 'virtual-raid-md-metadata' :
  raid10 ? 'virtual-raid-md10' : advanced ? 'virtual-raid-md10-advanced' : 'virtual-raid-md6';
const output = safePath(path.join(root, directory), root, false);
const log = fs.readFileSync(input, 'utf8').split(/\r?\n/);
const marker = zoned ? 'DISKSTUDIO_MD_ZONES_FIXTURE_PASS' :
  metadata ? 'DISKSTUDIO_MD_METADATA_FIXTURE_PASS' :
    raid10 ? 'DISKSTUDIO_MD_RAID10_FIXTURE_PASS' :
      advanced ? 'DISKSTUDIO_MD_RAID10_ADVANCED_FIXTURE_PASS' : 'DISKSTUDIO_MD6_FIXTURE_PASS';
if (!log.includes(marker)) throw new Error('Linux fixture did not pass');
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');

function decode(marker, length) {
  const prefix = marker + '_MANIFEST ';
  const header = log.filter(line => line.startsWith(prefix));
  if (header.length !== 1 || !log.includes(marker + '_PASS')) {
    throw new Error(`Missing Linux fixture ${marker}`);
  }
  const { bytes, sha256: expected, gzipSha256 } = JSON.parse(header[0].slice(prefix.length));
  const blocks = log.filter(line => line.startsWith(marker + '_BLOCK '));
  const compressed = Buffer.concat(blocks.map((line, index) => {
    const [tag, number, encoded] = line.split(' ');
    if (tag !== marker + '_BLOCK' || number !== String(index).padStart(5, '0') ||
        !/^[A-Za-z0-9+/]+={0,2}$/.test(encoded ?? '')) {
      throw new Error(`Invalid block for ${marker}`);
    }
    return Buffer.from(encoded, 'base64');
  }));
  if (bytes !== length || sha256(compressed) !== gzipSha256) {
    throw new Error(`Unexpected size or compressed hash for ${marker}`);
  }
  const contents = gunzipSync(compressed, { maxOutputLength: length });
  if (contents.length !== length || sha256(contents) !== expected) {
    throw new Error(`Decoded hash mismatch for ${marker}`);
  }
  return contents;
}

const proofs = log.filter(line => /^DS_MD6_PROOF [a-f0-9]{64}$/.test(line));
if (proofs.length !== 1) throw new Error('Missing independent filesystem readback hash');
const proofSha256 = proofs[0].slice('DS_MD6_PROOF '.length);

if (metadata || zoned || raid10 || advanced) {
  const counts = [['native', 5], ['raid5', 3], ['raid1', 2], ['raid0', 3], ['raid4', 3], ['raid10', 4]];
  const advancedCounts = [['mixed-far', 6], ['mixed-offset', 6], ['legacy-far', 6], ['fixed-mixed', 8]];
  const widths = advanced ? advancedCounts : raid10 ? ['n2', 'n3', 'f3', 'o2'].map(label => [label, 5]) : counts;
  const cases = zoned
    ? [['original', [20, 6, 13]], ['alternate', [20, 6, 13]]]
    : widths.map(([label, width]) => [label, Array(width).fill(16)]);
  const verified = cases.map(([label, memberMiB], index) => {
    const marker = `DS_MD_ARRAY_${(zoned ? 101 : 100) + index}`;
    const rows = log.filter(line => line.startsWith(marker + '_SIZE '));
    const size = Number(rows[0]?.split(' ').at(-1));
    const maxSize = memberMiB.reduce((sum, size) => sum + size, 0) * 1024 ** 2;
    if (rows.length !== 1 || !Number.isSafeInteger(size) || size <= 0 || size > maxSize) {
      throw new Error(`Invalid Linux logical size for ${label}`);
    }
    let logicalSha256;
    if (zoned || raid10 || advanced) {
      const digests = log.filter(line => line.startsWith(marker + '_SHA256 '));
      logicalSha256 = digests[0]?.split(' ').at(-1);
      if (digests.length !== 1 || !/^[a-f0-9]{64}$/.test(logicalSha256 ?? '')) {
        throw new Error(`Missing Linux logical SHA-256 for ${label}`);
      }
    }
    const tag = zoned ? 'ZONES' : raid10 ? 'RAID10' : advanced ? 'ADVANCED' : 'MEMBER';
    const members = memberMiB.map((size, slot) =>
      decode(`DS_MD_${tag}_${label}_${slot}`, size * 1024 ** 2));
    return { label, size, logicalSha256, members };
  });
  fs.mkdirSync(output);
  const arrays = verified.map(({ label, size, logicalSha256, members }) => {
    const files = members.map((contents, slot) => {
      const name = `Virtual-Raid-Md-${label}-${slot}.raw`;
      fs.writeFileSync(path.join(output, name), contents, { flag: 'wx' });
      return { name, sha256: sha256(contents) };
    });
    return { label, size, logicalSha256, files };
  });
  fs.writeFileSync(path.join(output, 'manifest.json'), JSON.stringify({
    files: arrays[zoned || advanced ? 1 : raid10 ? 3 : 0].files, arrays, proofSha256,
  }, null, 2) + '\n', { flag: 'wx' });
  console.log(`Linux fixture: native metadata and source bytes for ${arrays.length} arrays verified`);
  process.exit(0);
}

const layouts = [0, 1, 2, 3, 4, 5, 8, 9, 10, 16, 17, 18, 19, 20];
const vectors = layouts.map(layout => decode(`DS_MD6_VECTOR_${layout}`, 5 * 16 * 4096));
const members = Array.from({ length: 5 }, (_, slot) => decode(`DS_MD6_EXT4_${slot}`, 12 * 1024 ** 2));
fs.mkdirSync(output);
const files = members.map((contents, slot) => {
  const name = `Virtual-Raid-Md6-${slot}.raw`;
  fs.writeFileSync(path.join(output, name), contents, { flag: 'wx' });
  return { name, sha256: sha256(contents) };
});
for (const [index, layout] of layouts.entries()) {
  fs.writeFileSync(path.join(output, `vector-${layout}.raw`), vectors[index], { flag: 'wx' });
}
fs.writeFileSync(path.join(output, 'manifest.json'), JSON.stringify({
  files, proofSha256,
}, null, 2) + '\n', { flag: 'wx' });
console.log('Linux fixture: 14 layout vectors and five Ext4 array members verified');

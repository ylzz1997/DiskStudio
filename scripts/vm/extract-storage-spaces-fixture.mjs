import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { root, safePath } from './lab.mjs';

const [name] = process.argv.slice(2);
const match = /^storage-spaces-(metadata|witness(?:-full)?|internal|cache(?:-complete)?)-4kn-(\d+)\.log$/.exec(name ?? '');
if (!match || process.argv.length !== 3) throw Error('Expected the name of a Storage Spaces metadata or witness log');
const input = safePath(path.join(root, 'ubuntu', name));
const output = safePath(path.join(root, `storage-spaces-${match[1]}-${match[2]}`), root, false);
const mib = 1024 ** 2;
const sector = 4096;
const capacity = 16 * 1024 ** 3;
if (fs.statSync(input).size > 32 * mib) throw Error('Capture log exceeds its size bound');
const lines = fs.readFileSync(input, 'utf8').split(/\r?\n/);
if (!lines.includes('DISKSTUDIO_STORAGE_SPACES_METADATA_PASS')) throw Error('Guest capture did not pass');
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const header = 'DS_SPACES_MANIFEST ';
const manifests = lines.filter(line => line.startsWith(header)).map(line => JSON.parse(line.slice(header.length)));
if (manifests.length !== 2 || manifests[0]?.slot !== 'A' || manifests[1]?.slot !== 'B') {
  throw Error('Missing or duplicate A/B capture');
}

const aligned = value => Number.isSafeInteger(value) && value % sector === 0;
const verified = manifests.map(manifest => {
  const { slot, partition, regions, gzipBytes, gzipSha256 } = manifest;
  if (manifest.capacity !== capacity || manifest.sector !== sector || !Array.isArray(partition) ||
      partition.length !== 2 || !partition.every(aligned) || !Array.isArray(regions) ||
      regions.length < 3 || regions.length > 10) throw Error(`Invalid member ${slot} geometry`);
  const [start, end] = partition;
  const gptBytes = 64 * 1024;
  if (start < gptBytes || end > capacity - gptBytes || end - start < 32 * mib ||
      regions[0]?.offset !== 0 || regions[0]?.bytes !== gptBytes ||
      regions[1]?.offset !== capacity - gptBytes || regions[1]?.bytes !== gptBytes) {
    throw Error(`Invalid member ${slot} partition or GPT capture`);
  }
  let previous = start;
  for (const region of regions.slice(2)) {
    if (!aligned(region.offset) || !aligned(region.bytes) || region.bytes < sector ||
        region.bytes > mib || region.offset < previous || region.offset + region.bytes > end ||
        !(region.offset < start + 16 * mib || region.offset >= end - 16 * mib)) {
      throw Error(`Invalid member ${slot} metadata range`);
    }
    // Metadata windows may overlap when two headers occupy neighboring sectors.
    previous = region.offset + sector;
  }
  const witnesses = manifest.witnesses ?? [];
  if (!Array.isArray(witnesses) || witnesses.length > 128 ||
      (match[1].startsWith('witness') ? witnesses.length === 0 : witnesses.length !== 0)) {
    throw Error(`Unexpected member ${slot} witness list`);
  }
  previous = start;
  for (const witness of witnesses) {
    if (!aligned(witness.offset) || witness.offset <= previous || witness.offset + sector > end ||
        witness.bytes !== sector || ![1, 2].includes(witness.kind) || !aligned(witness.logical) ||
        witness.logical < 0 || witness.logical >= 1024 * mib) throw Error(`Invalid member ${slot} witness`);
    previous = witness.offset;
  }
  const internal = manifest.internal ?? [];
  const internalBlocks = match[1] === 'internal' ? [10, 11] : match[1].startsWith('cache') ? [1, 2, 3, 4, 5] : [];
  const internalSizes = match[1] === 'cache-complete' ? [64 * 1024, 4 * mib + 2 * sector, 64 * 1024, 64 * 1024, 64 * 1024] :
    match[1] === 'cache' ? [4 * mib, 4 * mib, 64 * 1024, 64 * 1024, 64 * 1024] : [4 * mib, 4 * mib];
  if (!Array.isArray(internal) || internal.length !== internalBlocks.length ||
      internal.some((region, index) => region?.offset !== start + (2 + internalBlocks[index]) * 256 * mib ||
        region.bytes !== internalSizes[index] ||
        region.offset + region.bytes > end)) {
    throw Error(`Invalid member ${slot} internal capture`);
  }
  const prefix = `DS_SPACES_BLOCK ${slot} `;
  const packed = Buffer.concat(lines.filter(line => line.startsWith(prefix)).map((line, index) => {
    const block = line.slice(prefix.length);
    const number = String(index).padStart(5, '0');
    if (!block.startsWith(number + ' ')) throw Error(`Out-of-order member ${slot} block`);
    const encoded = block.slice(number.length + 1);
    const bytes = Buffer.from(encoded, 'base64');
    if (bytes.length < 1 || bytes.length > 576 || bytes.toString('base64') !== encoded) {
      throw Error(`Invalid member ${slot} block`);
    }
    return bytes;
  }));
  if (packed.length !== gzipBytes || sha256(packed) !== gzipSha256) throw Error(`Member ${slot} archive mismatch`);
  const length = [...regions, ...internal].reduce((total, region) => total + region.bytes, 0) + witnesses.length * sector;
  const bytes = gunzipSync(packed, { maxOutputLength: length });
  if (bytes.length !== length) throw Error(`Member ${slot} capture is truncated`);
  let position = 0;
  for (const [index, region] of regions.entries()) {
    const contents = bytes.subarray(position, position + region.bytes);
    if (sha256(contents) !== region.sha256 ||
        (index >= 2 && contents.subarray(0, 8).toString('ascii') !== 'SPACEDB ')) {
      throw Error(`Member ${slot} region mismatch`);
    }
    position += region.bytes;
  }
  for (const witness of witnesses) {
    const contents = bytes.subarray(position, position + sector);
    if (sha256(contents) !== witness.sha256 || contents.subarray(0, 16).toString('ascii') !== 'DS-SPACES-DATA1\0' ||
        contents[16] !== witness.kind || contents.readBigUInt64LE(24) !== BigInt(witness.logical)) {
      throw Error(`Member ${slot} witness digest or identity mismatch`);
    }
    position += sector;
  }
  for (const region of internal) {
    if (sha256(bytes.subarray(position, position + region.bytes)) !== region.sha256) {
      throw Error(`Member ${slot} internal capture digest mismatch`);
    }
    position += region.bytes;
  }
  return { manifest, packed };
});

fs.mkdirSync(output);
for (const { manifest, packed } of verified) {
  fs.writeFileSync(path.join(output, `${manifest.slot}.gz`), packed, { flag: 'wx' });
}
fs.writeFileSync(path.join(output, 'manifest.json'), JSON.stringify(manifests, null, 2) + '\n', { flag: 'wx' });
console.log(`Verified two 4Kn Storage Spaces metadata captures in ${output}`);

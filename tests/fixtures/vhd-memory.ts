// Independent test writer for small VHD v1 byte arrays. No filesystem/device I/O.
export const MiB = 1024 ** 2;
const dv = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
export const be32 = (b: Uint8Array, at: number, n: number) =>
  dv(b).setUint32(at, n);
export const be64 = (b: Uint8Array, at: number, n: bigint | number) =>
  dv(b).setBigUint64(at, BigInt(n));
export function seal(b: Uint8Array, field: number) {
  b.fill(0, field, field + 4);
  be32(b, field, ~b.reduce((sum, value) => sum + value, 0) >>> 0);
}
export function logicalDisk() {
  const b = new Uint8Array(8 * MiB);
  b[510] = 0x55;
  b[511] = 0xaa;
  b[450] = 0x07;
  dv(b).setUint32(454, 2048, true);
  dv(b).setUint32(458, 4096, true);
  // Distinctive data in every block, including both sides of boundaries.
  for (let at = 4096; at < b.length; at += 511) b[at] = (at % 251) + 1;
  return b;
}
function footer(size: number, type: number) {
  const b = new Uint8Array(512);
  b.set(new TextEncoder().encode("conectix"));
  be32(b, 8, 2);
  be32(b, 12, 0x10000);
  be64(b, 16, type === 2 ? 0xffffffffffffffffn : 512);
  b.set(new TextEncoder().encode("test"), 28);
  be64(b, 40, size);
  be64(b, 48, size);
  be32(b, 60, type);
  b.fill(0x42, 68, 84);
  seal(b, 64);
  return b;
}
export function fixedVhd(raw = logicalDisk()) {
  const b = new Uint8Array(raw.length + 512);
  b.set(raw);
  b.set(footer(raw.length, 2), raw.length);
  return b;
}
export function dynamicVhd(
  raw = logicalDisk(),
  sparse = false,
  blockSize = 2 * MiB,
) {
  const entries = Math.ceil(raw.length / blockSize);
  const bitmap = Math.ceil(blockSize / 512 / 8 / 512) * 512;
  const tableSize = Math.ceil((entries * 4) / 512) * 512;
  const dataAt = 1536 + tableSize;
  const selected = Array.from({ length: entries }, (_, i) => i).filter(
    (i) => !sparse || i % 2 === 0,
  );
  const b = new Uint8Array(
    dataAt + selected.length * (bitmap + blockSize) + 512,
  );
  b.set(footer(raw.length, 3));
  b.set(b.subarray(0, 512), b.length - 512);
  const h = b.subarray(512, 1536);
  h.set(new TextEncoder().encode("cxsparse"));
  be64(h, 8, 0xffffffffffffffffn);
  be64(h, 16, 1536);
  be32(h, 24, 0x10000);
  be32(h, 28, entries);
  be32(h, 32, blockSize);
  seal(h, 36);
  b.fill(0xff, 1536, 1536 + entries * 4);
  // Reversed physical allocation order exercises BAT translation.
  selected.reverse().forEach((block, order) => {
    const at = dataAt + order * (bitmap + blockSize);
    be32(b, 1536 + block * 4, at / 512);
    b.fill(0xff, at, at + bitmap);
    b.set(
      raw.subarray(block * blockSize, (block + 1) * blockSize),
      at + bitmap,
    );
  });
  return b;
}
export function memoryFile(bytes: Uint8Array, name = "memory.vhd") {
  const reads: [number, number][] = [];
  return {
    reads,
    file: {
      name,
      size: bytes.length,
      slice(start: number, end = bytes.length) {
        if (start < 0 || end > bytes.length || end < start)
          throw new Error("out of fixture range");
        reads.push([start, end]);
        return {
          arrayBuffer: async () =>
            Uint8Array.from(bytes.subarray(start, end)).buffer,
        };
      },
    } as unknown as File,
  };
}

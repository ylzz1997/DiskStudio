// Independent small in-memory VDI writer; excluded from the product bundle.
import { logicalDisk, MiB } from "./vhd-memory";
export const le32 = (b: Uint8Array, at: number, n: number) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength).setUint32(at, n, true);
export const le64 = (b: Uint8Array, at: number, n: number | bigint) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength).setBigUint64(
    at,
    BigInt(n),
    true,
  );
export const get32 = (b: Uint8Array, at: number) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength).getUint32(at, true);
export function vdiBytes(
  raw = logicalDisk(),
  options: {
    block?: number;
    extra?: number;
    sparse?: boolean;
    fixed?: boolean;
    plus?: boolean;
  } = {},
) {
  const block = options.block ?? MiB,
    extra = options.extra ?? 0;
  const blocks = Math.ceil(raw.length / block),
    mapAt = 512,
    dataAt = Math.ceil((mapAt + blocks * 4) / 512) * 512;
  const selected = Array.from({ length: blocks }, (_, i) => i)
    .filter((i) => !options.sparse || i % 3 === 0)
    .reverse();
  const b = new Uint8Array(dataAt + selected.length * (block + extra));
  le32(b, 64, 0xbeda107f);
  le32(b, 68, 0x10001);
  le32(b, 72, options.plus ? 400 : 384);
  le32(b, 76, options.fixed ? 2 : 1);
  le32(b, 340, mapAt);
  le32(b, 344, dataAt);
  le32(b, 360, 512);
  le64(b, 368, raw.length);
  le32(b, 376, block);
  le32(b, 380, extra);
  le32(b, 384, blocks);
  le32(b, 388, selected.length);
  b.fill(0x42, 392, 408);
  b.fill(0x24, 408, 424);
  if (options.plus) le32(b, 468, 512);
  for (let i = 0; i < blocks; i++)
    le32(b, mapAt + i * 4, i % 3 === 1 ? 0xffffffff : 0xfffffffe);
  selected.forEach((i, slot) => {
    le32(b, mapAt + i * 4, slot);
    const at = dataAt + slot * (block + extra);
    b.fill(0xa5, at, at + extra);
    b.set(raw.subarray(i * block, (i + 1) * block), at + extra);
  });
  return b;
}

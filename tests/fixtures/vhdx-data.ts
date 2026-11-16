import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
export const MiB = 1024 ** 2,
  K = 1024,
  BAT = 2 * MiB,
  META = 3 * MiB;
export const fixture = (name = "dynamic") =>
  Uint8Array.from(
    gunzipSync(readFileSync(new URL(`vhdx-qemu-${name}.gz`, import.meta.url))),
  );
export const rawFixture = (fourK = false) =>
  Uint8Array.from(
    gunzipSync(
      readFileSync(
        new URL(
          fourK ? "vhdx-gpt4k-raw.gz" : "vhd-qemu-raw.gz",
          import.meta.url,
        ),
      ),
    ),
  );
const v = (b: Uint8Array) => new DataView(b.buffer, b.byteOffset, b.byteLength);
export const le32 = (b: Uint8Array, at: number, n: number) =>
  v(b).setUint32(at, n, true);
export const le64 = (b: Uint8Array, at: number, n: bigint | number) =>
  v(b).setBigUint64(at, BigInt(n), true);
export const get32 = (b: Uint8Array, at: number) => v(b).getUint32(at, true);
export const get64 = (b: Uint8Array, at: number) => v(b).getBigUint64(at, true);
// Independent bitwise CRC-32C writer, separate from production table algorithm.
export function seal(b: Uint8Array, at: number, length: number) {
  b.fill(0, at + 4, at + 8);
  let crc = 0xffffffff;
  for (const byte of b.subarray(at, at + length)) {
    crc ^= byte;
    for (let j = 0; j < 8; j++) crc = (crc >>> 1) ^ (crc & 1 ? 0x82f63b78 : 0);
  }
  le32(b, at + 4, ~crc >>> 0);
}
export function item(b: Uint8Array, index: number) {
  return META + get32(b, META + 32 + index * 32 + 16);
}

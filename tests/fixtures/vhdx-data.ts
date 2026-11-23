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

const LOG_ID = Uint8Array.from({ length: 16 }, (_, i) => 0x15 + i);
const SECTOR = 4096;
export function markActiveLog(bytes: Uint8Array) {
  for (const header of [64 * K, 128 * K]) {
    if (get64(bytes, header + 72) !== BigInt(MiB) || get32(bytes, header + 68) !== MiB)
      throw Error("Expected a 1 MiB log at byte 1048576 in the test fixture");
    bytes.set(LOG_ID, header + 48);
    seal(bytes, header, SECTOR);
  }
}

export function logEntry(
  bytes: Uint8Array,
  ringAt: number,
  tail: number,
  sequence: bigint,
  updates: { target: number; data: Uint8Array | number }[],
  flushed = bytes.length,
  last = bytes.length,
) {
  const initial = Math.ceil((64 + 32 * updates.length) / SECTOR) * SECTOR;
  const entry = new Uint8Array(initial + SECTOR * updates.filter(({ data }) => typeof data !== "number").length);
  entry.set(Buffer.from("loge"));
  le32(entry, 8, entry.length);
  le32(entry, 12, tail);
  le64(entry, 16, sequence);
  le32(entry, 24, updates.length);
  entry.set(LOG_ID, 32);
  le64(entry, 48, flushed);
  le64(entry, 56, last);
  let block = initial;
  updates.forEach(({ target, data }, i) => {
    const at = 64 + i * 32;
    le64(entry, at + 16, target);
    le64(entry, at + 24, sequence);
    if (typeof data === "number") {
      entry.set(Buffer.from("zero"), at);
      le64(entry, at + 8, data);
    } else {
      if (data.length !== SECTOR) throw Error("Expected one full log data sector");
      entry.set(Buffer.from("desc"), at);
      entry.set(data.subarray(0, 8), at + 8);
      entry.set(data.subarray(SECTOR - 4), at + 4);
      entry.set(Buffer.from("data"), block);
      le32(entry, block + 4, Number(sequence >> 32n));
      entry.set(data.subarray(8, SECTOR - 4), block + 8);
      le32(entry, block + SECTOR - 4, Number(sequence & 0xffffffffn));
      block += SECTOR;
    }
  });
  seal(entry, 0, entry.length);
  const first = Math.min(entry.length, MiB - ringAt);
  bytes.set(entry.subarray(0, first), MiB + ringAt);
  bytes.set(entry.subarray(first), MiB);
}
